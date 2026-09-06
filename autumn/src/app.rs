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

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::any::{Any, TypeId};
use std::collections::{BTreeSet, HashMap, HashSet};
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
        plugin_contracts: Vec::new(),
        plugin_config_roots: BTreeSet::new(),
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
        alert_channels: Vec::new(),
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
        mail_suppression_store: None,
        #[cfg(feature = "mail")]
        mount_unsubscribe_endpoint: false,
        #[cfg(feature = "mail")]
        mail_previews: Vec::new(),
        #[cfg(feature = "maud")]
        story_gallery: None,
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

/// Count the raw routers omitted from `autumn routes` output because their
/// endpoints can't be enumerated — the value `autumn routes audit` treats as a
/// hard failure (an unprovable route defeats the coverage guarantee).
///
/// Every `.merge()` router is rootless — it has no mount prefix to match
/// declarations against — so it is always opaque and always counts. A `.nest()`
/// router carries a mount prefix, so it is treated as **covered** (enumerable,
/// not omitted) when at least one declared route (from
/// [`declare_plugin_routes`](AppBuilder::declare_plugin_routes)) has a path that
/// falls under that prefix. This makes the documented
/// `app.nest(prefix, router).declare_plugin_routes(routes)` pattern audit-clean
/// without any dedicated bookkeeping: the declared routes prove the mount.
///
/// Soundness (fail-closed) is preserved: a bare `nest(prefix, raw_router)` with
/// no declared route under `prefix` stays uncovered and counts, and every
/// `merge()` counts unconditionally.
fn omitted_router_count<'a>(
    merge_routers: usize,
    nest_prefixes: impl IntoIterator<Item = &'a str>,
    declared_routes: &[crate::route_listing::RouteInfo],
) -> usize {
    let uncovered_nests = nest_prefixes
        .into_iter()
        .filter(|prefix| !nest_prefix_is_covered(prefix, declared_routes))
        .count();
    merge_routers + uncovered_nests
}

/// A nested mount at `prefix` is "covered" when at least one declared route's
/// path falls under that prefix, proving the nested router's endpoints are
/// enumerable in the `autumn routes` listing.
fn nest_prefix_is_covered(
    prefix: &str,
    declared_routes: &[crate::route_listing::RouteInfo],
) -> bool {
    declared_routes
        .iter()
        .any(|route| path_is_under_prefix(&route.path, prefix))
}

/// Whether `path` is mounted under `prefix` — i.e. equal to the prefix or a
/// descendant of it at a path-segment boundary (`/admin` covers `/admin` and
/// `/admin/users`, but not `/administrators`). A root prefix (`/` or empty)
/// covers everything.
fn path_is_under_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
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
    /// Compatibility contracts declared by the plugins applied to this builder
    /// (issue #1601), in registration order. Emitted after
    /// [`PLUGIN_CONTRACT_MARKER`](crate::plugin_contract::PLUGIN_CONTRACT_MARKER)
    /// by the route dump so `autumn plugin-check` can report experimental
    /// surface use without linking the plugin itself.
    pub(crate) plugin_contracts: Vec<crate::plugin_contract::PluginContract>,
    /// Top-level config roots plugins have declared as their own opaque config
    /// sections via [`config_section`](AppBuilder::config_section). Threaded into
    /// the default config loader so `server.strict_config` treats them as
    /// known-and-opaque instead of unknown-key hard errors.
    pub(crate) plugin_config_roots: BTreeSet<String>,
    /// Custom error page renderer (overrides built-in pages).
    #[cfg(feature = "maud")]
    error_page_renderer: Option<SharedRenderer>,
    /// Embedded Diesel migrations, registered via `.migrations()` (tagged
    /// `"app"`) or [`Self::plugin_migrations`] (tagged with the caller's
    /// `name`) — see [`Self::plugin_migrations`] for why the name matters.
    #[cfg(feature = "db")]
    migrations: Vec<(&'static str, migrate::EmbeddedMigrations)>,
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
    /// `build_session_layer` skips the config-driven `memory`/`redis` selection
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
    /// Operator-alert channels registered via [`AppBuilder::with_alert_channel`].
    /// Combined with the built-in mail/webhook channels derived from
    /// `[alerts]` config and installed onto `AppState` so the built-in
    /// condition hooks can fan out to each. Empty means only config-derived
    /// destinations are used. See [`crate::alerts`].
    pub(crate) alert_channels: Vec<Arc<dyn crate::alerts::AlertChannel>>,
    /// `OpenAPI` generation configuration. When `Some`, the router mounts
    /// `/openapi.json` (serving the generated spec) and `/swagger-ui` (if the
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
    pub(crate) mail_suppression_store: Option<crate::mail::suppression::SuppressionStoreHandle>,
    #[cfg(feature = "mail")]
    pub(crate) mount_unsubscribe_endpoint: bool,
    /// Mail template previews registered for the dev preview UI.
    #[cfg(feature = "mail")]
    mail_previews: Vec<crate::mail::MailPreview>,
    /// Widget story gallery registered for the `/_stories` UI (#1526).
    #[cfg(feature = "maud")]
    story_gallery: Option<crate::stories::StoryGallery>,
    /// Routes explicitly declared by plugins for listing purposes, to complement
    /// opaque `nest_routers`. Included in `autumn routes` output even though
    /// the underlying Axum router is not enumerable, and handed to the router
    /// build so the duplicate-route preflight can see inside those otherwise
    /// opaque mounts.
    ///
    /// `pub(crate)` so [`TestApp`](crate::test::TestApp) can carry them too — a
    /// harness that dropped them would mount a colliding plugin cleanly in
    /// tests and panic at boot in production.
    pub(crate) declared_routes: Vec<crate::route_listing::RouteInfo>,
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

/// The one service type every user-registered layer is composed against.
///
/// Erasing to a single concrete service type is what lets an arbitrary number
/// of operator layers be folded into ONE `Router::layer` call: a `tower-layer`
/// tuple needs its members' types known at compile time, and a `Vec` of
/// registrations does not have that (#2198).
///
/// # When you need to name this type
///
/// Almost never. A layer that is generic over the service it wraps — the shape
/// of every layer in tower, tower-http, and this repo — satisfies
/// [`AppBuilder::layer`]'s bounds without mentioning it. The exception is a
/// layer deliberately written against ONE concrete inner service type. Before
/// #2198 that target was `axum::routing::Route`; it is now this alias:
///
/// ```rust,ignore
/// impl tower::Layer<ErasedAppService> for MyRouteSpecificLayer { /* … */ }
/// ```
///
/// Prefer the generic form (`impl<S> tower::Layer<S> for MyLayer`) unless the
/// layer genuinely cannot be written that way.
pub type ErasedAppService = tower::util::BoxCloneSyncService<
    axum::extract::Request,
    axum::response::Response,
    std::convert::Infallible,
>;

/// A user-registered layer with its type erased at registration time, so a
/// heterogeneous `Vec` of them can be composed into a single application.
pub(crate) type ErasedAppLayer = tower::util::BoxCloneSyncServiceLayer<
    ErasedAppService,
    axum::extract::Request,
    axum::response::Response,
    std::convert::Infallible,
>;

/// Metadata and the type-erased layer for a user-registered middleware.
pub(crate) struct CustomLayerRegistration {
    /// Concrete type for the registered layer.
    pub(crate) type_id: TypeId,
    /// Concrete type name for generic layer families that need router-time
    /// classification without unstable specialization.
    pub(crate) type_name: &'static str,
    /// The registered layer, erased so registrations of unrelated types can
    /// share one `Vec` and one `Router::layer` application.
    pub(crate) layer: ErasedAppLayer,
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
///
/// The layer is applied to Autumn's own erased ingress service rather than to
/// `axum::routing::Route` directly, so registrations of unrelated types can be
/// composed into a single `Router::layer` call (#2198). In practice this is
/// invisible: a real-world layer is generic over its inner service and
/// satisfies both.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a usable Autumn app-wide Tower layer",
    label = "this type is not a `tower::Layer` over Autumn's ingress service with the required service bounds",
    note = "`AppBuilder::layer(..)` requires a layer that is generic over the service it wraps (or written against `autumn_web::app::ErasedAppService`), producing:\n    L::Service: Service<axum::extract::Request, Response = axum::response::Response, Error = Infallible> + Clone + Send + Sync + 'static,\n    <L::Service as Service<axum::extract::Request>>::Future: Send + 'static\nand the layer itself must be Clone + Send + Sync + 'static.\nSee docs/guide/middleware.md for common patterns and how to wrap raw-error layers (e.g. TimeoutLayer) with HandleErrorLayer."
)]
pub trait IntoAppLayer: sealed::Sealed + Send + Sync + 'static {
    /// Erase this layer's type so it can be stored alongside registrations of
    /// other types and composed into one application. Not intended for direct
    /// use.
    #[doc(hidden)]
    fn erase(self) -> ErasedAppLayer;
}

impl<L> sealed::Sealed for L
where
    L: tower::Layer<ErasedAppService> + Clone + Send + Sync + 'static,
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
    L: tower::Layer<ErasedAppService> + Clone + Send + Sync + 'static,
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
    fn erase(self) -> ErasedAppLayer {
        tower::util::BoxCloneSyncServiceLayer::new(self)
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
    /// `OpenAPI` 3.1 JSON document at `OpenApiConfig::openapi_json_path`
    /// (default `/openapi.json`). If
    /// `OpenApiConfig::swagger_ui_path` is set (default `/swagger-ui`),
    /// a Swagger UI HTML page is served there too.
    ///
    /// Routes marked `#[api_doc(hidden)]` are excluded.
    ///
    /// Narrative guide: `docs/guide/openapi.md`.
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
    ///     .scoped("/api", RequestIdLayer::default(), routes![list_users])
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
    /// See [the middleware guide](https://github.com/autumn-foundation/autumn/blob/trunk/docs/guide/middleware.md)
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
            layer: layer.erase(),
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
            layer: layer.erase(),
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
    ///
    /// Declaring routes also makes a [`nest`](Self::nest) mount *coverage-clean*
    /// for `autumn routes audit`: a nested router is normally opaque and counts
    /// as an omitted, unprovable router that hard-fails the gate, but when at
    /// least one declared route's path falls under the nest's prefix, the mount
    /// is treated as enumerable and no longer counts. So the documented
    /// `app.nest(prefix, router).declare_plugin_routes(routes)` pattern — with
    /// `routes` covering everything the raw router serves under `prefix` — passes
    /// the audit. A bare `nest`/`merge` with no covering declaration stays
    /// opaque and still fails closed.
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

    /// The route manifest this builder would dump for `autumn routes` —
    /// enumerable routes plus everything declared via
    /// [`declare_plugin_routes`](Self::declare_plugin_routes).
    ///
    /// This is the seam a plugin author's conformance test needs: it runs the
    /// same collection the `AUTUMN_DUMP_ROUTES` path runs, in-process, so
    /// `autumn_web::plugin_conformance::run_conformance` can be pointed at a
    /// host app built in a test without compiling and executing a binary.
    ///
    /// Framework-owned routes (probes, actuator, docs) are **not** included:
    /// they depend on the loaded configuration, and a conformance run is about
    /// what the plugin contributes.
    ///
    /// # Errors
    ///
    /// Returns [`RouterBuildError::UnregisteredApiVersion`](crate::RouterBuildError::UnregisteredApiVersion)
    /// if a route names an API version this builder has not registered — the
    /// same refusal `autumn routes` reports.
    pub fn plugin_route_infos(
        &self,
    ) -> Result<Vec<crate::route_listing::RouteInfo>, crate::RouterBuildError> {
        let mut infos = crate::route_listing::collect_route_infos(
            &self.routes,
            &self.route_sources,
            &self.scoped_groups,
            &self.api_versions,
        )?;
        infos.extend(self.declared_routes.iter().cloned());
        Ok(infos)
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

    /// Designate a block of typed in-memory state to survive an in-place
    /// upgrade (issue #1674).
    ///
    /// On `SIGUSR2` the block is snapshotted, frozen against further writes,
    /// and handed to the successor build along with the listening socket; the
    /// successor installs it before it serves its first request. On an ordinary
    /// cold start `initial` is used.
    ///
    /// Reach the block from a handler with
    /// [`AppState::live_state`](crate::AppState::live_state). Use
    /// [`with_live_state_from`](Self::with_live_state_from) when the new build
    /// changed the shape.
    ///
    /// One block per app: designating a second is a startup error, because
    /// silently carrying only one of them is exactly the data loss this feature
    /// exists to prevent.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::prelude::*;
    /// use autumn_web::upgrade::LiveState;
    /// use serde::{Deserialize, Serialize};
    ///
    /// #[derive(Default, Serialize, Deserialize)]
    /// struct Stats { hits: u64 }
    /// impl LiveState for Stats { const VERSION: u32 = 1; }
    ///
    /// # #[get("/")] async fn index() -> &'static str { "ok" }
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// autumn_web::app()
    ///     .routes(routes![index])
    ///     .with_live_state(Stats::default())
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    #[must_use]
    pub fn with_live_state<T>(self, initial: T) -> Self
    where
        T: crate::upgrade::LiveState,
    {
        self.state_initializer(move |state| {
            install_live_state(state, initial, crate::upgrade::decode::<T>);
        })
    }

    /// Designate a live-state block whose shape changed since the previous
    /// build, carrying an `Old` snapshot across through the
    /// [`state_migration!`](crate::state_migration) declared for it.
    ///
    /// A snapshot at `T`'s own version is adopted directly; one at `Old`'s
    /// version is migrated; anything else refuses to start, which aborts the
    /// upgrade and leaves the previous build serving.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::prelude::*;
    /// use autumn_web::state_migration;
    /// use autumn_web::upgrade::LiveState;
    /// use serde::{Deserialize, Serialize};
    ///
    /// #[derive(Serialize, Deserialize)]
    /// struct StatsV1 { hits: u64 }
    /// #[derive(Default, Serialize, Deserialize)]
    /// struct Stats { hits: u64, upgrades: u64 }
    /// impl LiveState for StatsV1 { const VERSION: u32 = 1; }
    /// impl LiveState for Stats { const VERSION: u32 = 2; }
    ///
    /// state_migration! {
    ///     from StatsV1 as old => Stats {
    ///         hits: old.hits,
    ///         upgrades: 1,
    ///     }
    /// }
    ///
    /// # #[get("/")] async fn index() -> &'static str { "ok" }
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// autumn_web::app()
    ///     .routes(routes![index])
    ///     .with_live_state_from::<StatsV1, _>(Stats::default())
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    #[must_use]
    pub fn with_live_state_from<Old, T>(self, initial: T) -> Self
    where
        Old: crate::upgrade::LiveState,
        T: crate::upgrade::MigrateFrom<Old>,
    {
        self.state_initializer(move |state| {
            install_live_state(state, initial, crate::upgrade::decode_migrating::<Old, T>);
        })
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

    /// Enable **user impersonation** for this app, gated by `gate`.
    ///
    /// Impersonation is default-deny: without this call (or
    /// `AdminPlugin::with_impersonation`, which does it for you)
    /// [`begin_impersonation`](crate::auth::impersonation::begin_impersonation)
    /// refuses every attempt with `403`. It also requires an audit sink — see
    /// [`with_audit_sink`](Self::with_audit_sink).
    ///
    /// ```rust,no_run
    /// use autumn_web::auth::impersonation::ImpersonationGate;
    ///
    /// # fn wire(app: autumn_web::app::AppBuilder) -> autumn_web::app::AppBuilder {
    /// app.impersonation_gate(ImpersonationGate::allow_roles(["admin"]))
    /// # }
    /// ```
    #[must_use]
    pub fn impersonation_gate(self, gate: crate::auth::impersonation::ImpersonationGate) -> Self {
        self.state_initializer(move |state| {
            // Surface a self-destructive `[auth].session_key` at boot rather
            // than at the first impersonation attempt, which refuses outright.
            let auth_key = state.auth_session_key();
            if crate::auth::impersonation::is_reserved_session_key(auth_key) {
                tracing::error!(
                    auth_session_key = %auth_key,
                    "impersonation is enabled but `auth.session_key` collides with a key the \
                     impersonation record reserves; every attempt will be refused"
                );
            }
            state.insert_extension(gate);
        })
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
    // user-provided trait impl. The defaults preserve current behaviour, so an
    // app that does not customize sees no change. Plugins chain these in
    // `build()` to ship a subsystem — an `AwsSecretsConfigPlugin` calling
    // `app.with_config_loader(...)`. See `docs/guides/extensibility.md`.

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

    /// Provide the key/value seam an `#[edge]` handler reads at the origin
    /// (issue #1790).
    ///
    /// An `#[edge(needs(kv))]` handler takes an
    /// [`EdgeCache`](autumn_edge::EdgeCache) extractor. At the edge the capsule
    /// runtime injects that handle per request; at the origin this method does,
    /// installing it as an app-wide extension layer so the *same handler
    /// source* serves from both substrates. Without it the extractor declines
    /// with an actionable `500` naming this call.
    ///
    /// Most apps pass [`CacheEdgeKv`](crate::CacheEdgeKv) over the cache they
    /// already run, so anything the origin publishes with
    /// [`cache::insert_cached`](crate::cache::insert_cached) is visible to the
    /// edge lane; any other [`EdgeKv`](autumn_edge::EdgeKv) works just as well.
    ///
    /// # Not a database
    ///
    /// The seam is a non-authoritative, read-only replica (ADR-0004 category
    /// 2): a miss is always a legal answer and staleness is expected. Routes
    /// whose correctness depends on a fresh, authoritative read belong on the
    /// origin — see [`edge_support`](crate::edge_support).
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::sync::Arc;
    ///
    /// use autumn_web::CacheEdgeKv;
    /// use autumn_web::cache::{Cache, MokaCache};
    /// use autumn_web::edge::EdgeKv;
    /// use autumn_web::prelude::*;
    ///
    /// # #[get("/health")] async fn health() -> &'static str { "ok" }
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// let cache = Arc::new(MokaCache::new(1_024, None)) as Arc<dyn Cache>;
    ///
    /// autumn_web::app()
    ///     .routes(routes![health])
    ///     .with_edge_kv(Arc::new(CacheEdgeKv::new(cache)) as Arc<dyn EdgeKv>)
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    #[cfg(feature = "edge")]
    #[must_use]
    pub fn with_edge_kv(self, kv: Arc<dyn autumn_edge::EdgeKv>) -> Self {
        // `EdgeCache::layer` hands back a ready-made `axum::Extension`, which is
        // itself a `tower::Layer` and therefore an `IntoAppLayer` — the same
        // plumbing `install_i18n_bundle_layer` uses. No adapter needed, and the
        // injection type stays an implementation detail of `autumn-edge`.
        self.layer(autumn_edge::EdgeCache::layer(kv))
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

    /// Register an operator-alert delivery channel.
    ///
    /// Alerts for the built-in conditions (dead-lettered jobs, Down health
    /// indicators, 5xx-rate spikes, scheduled-task failures) are delivered to
    /// every registered [`AlertChannel`](crate::alerts::AlertChannel) **plus**
    /// the built-in mail/webhook channels derived from `[alerts]` config.
    ///
    /// This is the extension seam for additional transports (`PagerDuty`, Slack,
    /// Discord — follow-up #1630): implement
    /// [`AlertChannel`](crate::alerts::AlertChannel) and register it here. The
    /// framework core never changes. Most apps need no code at all — configuring
    /// an `email` and/or `webhook_url` under `[alerts]` is sufficient.
    ///
    /// ```rust,no_run
    /// use autumn_web::alerts::{Alert, AlertChannel, AlertDeliveryError, AlertDeliveryFuture};
    ///
    /// struct PagerDuty;
    /// impl AlertChannel for PagerDuty {
    ///     fn name(&self) -> &'static str { "pagerduty" }
    ///     fn deliver<'a>(&'a self, alert: &'a Alert) -> AlertDeliveryFuture<'a> {
    ///         Box::pin(async move {
    ///             let _ = (&alert.dedup_key, alert.severity);
    ///             Ok::<(), AlertDeliveryError>(())
    ///         })
    ///     }
    /// }
    ///
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// autumn_web::app()
    ///     .with_alert_channel(PagerDuty)
    /// #   .routes(vec![])
    /// #   ;
    /// # }
    /// ```
    #[must_use]
    pub fn with_alert_channel<C: crate::alerts::AlertChannel>(mut self, channel: C) -> Self {
        self.alert_channels
            .push(Arc::new(channel) as Arc<dyn crate::alerts::AlertChannel>);
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

    /// Register a notification store, overriding the default resolution used
    /// by the [`Notifications`] extractor.
    ///
    /// Without this call the extractor resolves its store automatically: the
    /// database-backed [`DbNotificationStore`] when a pool is configured (the
    /// `notifications` table is scaffolded by `autumn generate
    /// notifications`), the process-local
    /// [`MemoryNotificationStore`](crate::notifications::MemoryNotificationStore)
    /// otherwise. Register a store explicitly to plug in a custom backend.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use autumn_web::notifications::MemoryNotificationStore;
    ///
    /// autumn_web::app()
    ///     .with_notification_store(MemoryNotificationStore::new())
    ///     .run()
    ///     .await;
    /// ```
    ///
    /// [`Notifications`]: crate::notifications::Notifications
    /// [`DbNotificationStore`]: crate::notifications::DbNotificationStore
    #[must_use]
    pub fn with_notification_store<S>(self, store: S) -> Self
    where
        S: crate::notifications::NotificationStore,
    {
        let service = crate::notifications::Notifications::new(store);
        self.state_initializer(move |state| {
            state.insert_extension(service);
        })
    }

    /// Register a push subscription store, overriding the default resolution
    /// used by the [`WebPush`] extractor.
    ///
    /// Without this call the extractor resolves its store automatically: the
    /// database-backed
    /// [`DbPushSubscriptionStore`](crate::push::DbPushSubscriptionStore) when a
    /// pool is configured (the `push_subscriptions` table is scaffolded by
    /// `autumn generate pwa`), the process-local
    /// [`MemoryPushSubscriptionStore`](crate::push::MemoryPushSubscriptionStore)
    /// otherwise.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use autumn_web::push::MemoryPushSubscriptionStore;
    ///
    /// autumn_web::app()
    ///     .with_push_subscription_store(MemoryPushSubscriptionStore::new())
    ///     .merge(autumn_web::push::router())
    ///     .run()
    ///     .await;
    /// ```
    ///
    /// [`WebPush`]: crate::push::WebPush
    #[must_use]
    pub fn with_push_subscription_store<S>(self, store: S) -> Self
    where
        S: crate::push::PushSubscriptionStore,
    {
        self.state_initializer(move |state| {
            state.insert_extension(crate::push::WebPush::from_state_with_store(state, store));
        })
    }

    /// Register a fully-built [`WebPush`] service, overriding key, store,
    /// transport, TTL and clock at once.
    ///
    /// This is the registration path for a **custom
    /// [`PushTransport`]** — routing push traffic through your own HTTP stack
    /// or a queue rather than the built-in one — and for supplying a VAPID key
    /// from somewhere `[push] private_key` cannot reach, such as a secrets
    /// manager fetched at boot.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use autumn_web::push::{MemoryPushSubscriptionStore, VapidKey, WebPush};
    ///
    /// let push = WebPush::new(
    ///     MemoryPushSubscriptionStore::new(),
    ///     VapidKey::from_base64url(&fetch_key_from_vault().await?)?,
    ///     "mailto:ops@example.com",
    ///     MyQueueTransport::new(),
    /// );
    ///
    /// autumn_web::app()
    ///     .with_web_push(push)
    ///     .merge(autumn_web::push::router())
    ///     .run()
    ///     .await;
    /// ```
    ///
    /// [`WebPush`]: crate::push::WebPush
    /// [`PushTransport`]: crate::push::PushTransport
    #[must_use]
    pub fn with_web_push(self, push: crate::push::WebPush) -> Self {
        self.state_initializer(move |state| {
            state.insert_extension(push);
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

    /// Register a bounce/complaint
    /// [`SuppressionStore`](crate::mail::suppression::SuppressionStore) so
    /// [`Mailer::send`](crate::mail::Mailer::send) skips addresses that have
    /// hard-bounced or complained (issue #1247).
    ///
    /// Zero-config apps need not call this: the framework wires an in-memory
    /// default store automatically. Use this to plug the durable
    /// [`PgSuppressionStore`](crate::mail::suppression::PgSuppressionStore) (or
    /// a custom backend) for multi-instance deploys that must share suppression
    /// across replicas. Mirrors [`Self::with_suppression_store`].
    #[cfg(feature = "mail")]
    #[must_use]
    pub fn with_mail_suppression_store(
        mut self,
        store: impl crate::mail::suppression::SuppressionStore + 'static,
    ) -> Self {
        self.mail_suppression_store =
            Some(crate::mail::suppression::SuppressionStoreHandle::new(store));
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

    /// Register the widget story gallery served at `/_stories` (#1526).
    ///
    /// Routes mount only when the resolved config has `[stories] enabled =
    /// true` (off by default, opt-in per profile). Start from
    /// [`StoryGallery::builtin`](crate::stories::StoryGallery::builtin) for
    /// the framework widget set and
    /// [`extend`](crate::stories::StoryGallery::extend) it with your app's
    /// own `story!{...}` entries. See `docs/guide/stories.md`.
    #[cfg(feature = "maud")]
    #[must_use]
    pub fn with_story_gallery(mut self, gallery: crate::stories::StoryGallery) -> Self {
        self.story_gallery = Some(gallery);
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
        if let Some(mut contract) = plugin.contract() {
            Self::enforce_plugin_contract(&contract);
            // Route attribution keys on `Plugin::name()` while a contract names
            // the plugin's CRATE; the default `name()` is `type_name`, so a
            // plugin that declares `env!("CARGO_PKG_NAME")` without overriding
            // `name()` has two identities. Carry both so
            // `autumn plugin-check --plugin-name` finds it under either.
            contract.registered_as = Some(name.as_ref().to_owned());
            self.plugin_contracts.push(contract);
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

    /// The compatibility contracts declared by the plugins applied so far
    /// (issue #1601), in registration order.
    ///
    /// A plugin that returns `None` from
    /// [`Plugin::contract`](crate::plugin::Plugin::contract) contributes
    /// nothing here, and a duplicate registration contributes once — the
    /// duplicate is skipped before its contract is read.
    #[must_use]
    pub fn plugin_contracts(&self) -> &[crate::plugin_contract::PluginContract] {
        &self.plugin_contracts
    }

    /// Check one plugin's declared `autumn-web` range against the framework it
    /// is actually compiled into.
    ///
    /// An incompatible pairing **panics** at registration: the plugin is about
    /// to wire itself into an application built on a framework it does not
    /// claim to support, and the whole point of the contract is that this stops
    /// being a silent surprise. The message names both versions and both
    /// remedies.
    ///
    /// A requirement that cannot be parsed only warns. It is the *plugin
    /// author's* typo, and `autumn plugin-check` fails on it in their CI —
    /// hard-failing here would punish an application author for a mistake they
    /// cannot fix.
    ///
    /// # The escape hatch
    ///
    /// The one thing an application author *cannot* fix is a plugin whose
    /// declared range is merely stale — cargo has already proven the two link
    /// one `autumn-web`, so an over-tight literal in somebody else's crate
    /// should not be able to strand a working deployment. Setting
    /// `AUTUMN_PLUGIN_CONTRACT=warn` downgrades the panic to a `tracing::warn!`
    /// carrying the same message. It is named in the panic text itself, so the
    /// person who hits it does not have to find this doc first. Loud-by-default
    /// is the point; unbootable-with-no-recourse is not.
    ///
    /// Note that a **duplicate** registration is skipped before its contract is
    /// read, so enforcement applies to the first plugin registered under a
    /// given name.
    #[track_caller]
    fn enforce_plugin_contract(contract: &crate::plugin_contract::PluginContract) {
        use crate::plugin_contract::{AUTUMN_WEB_VERSION, ContractVerdict, evaluate};

        match evaluate(contract, AUTUMN_WEB_VERSION) {
            ContractVerdict::Compatible | ContractVerdict::Undeclared => {}
            ContractVerdict::Incompatible(err) => {
                if std::env::var("AUTUMN_PLUGIN_CONTRACT").as_deref() == Ok("warn") {
                    tracing::warn!(
                        plugin = contract.plugin.as_str(),
                        "{err}\n  (demoted to a warning by AUTUMN_PLUGIN_CONTRACT=warn)"
                    );
                } else {
                    panic!(
                        "{err}\n  \u{2192} or, to boot anyway while you sort it out, set \
                         AUTUMN_PLUGIN_CONTRACT=warn"
                    );
                }
            }
            ContractVerdict::Unparseable {
                requirement,
                reason,
            } => {
                tracing::warn!(
                    plugin = contract.plugin.as_str(),
                    requirement = requirement.as_str(),
                    reason = reason.as_str(),
                    "plugin declares an autumn-web requirement that cannot be evaluated; \
                     compatibility was NOT checked (run `autumn plugin-check` on the plugin)"
                );
            }
        }
    }

    /// Declare a plugin-owned top-level config section so it coexists with
    /// `server.strict_config = true`.
    ///
    /// Core's [`AutumnConfig`] schema is closed: any
    /// top-level `[root]` table it does not know about is an unknown key. Under
    /// `strict_config`, an unknown root is a **hard** boot error. A plugin that
    /// reads its own top-level table — for example `autumn-media-plugin` reading
    /// `[media]` via raw TOML — would therefore make a `strict_config` app fail
    /// at boot with `unknown key "media"`.
    ///
    /// Calling `config_section("media")` registers `[media]` as a **known,
    /// opaque** section: the strict unknown-key check accepts the root and does
    /// **not** validate its contents (the plugin owns that — core has no schema
    /// for it). The seam is **fail-closed**: only the roots a plugin explicitly
    /// declares are exempt; every other unknown top-level root still hard-fails,
    /// so a typo like `[medai]` is still caught.
    ///
    /// Call this from your [`Plugin::build`](crate::plugin::Plugin::build)
    /// implementation, where the plugin is applied to the builder:
    ///
    /// ```ignore
    /// impl Plugin for MediaPlugin {
    ///     fn build(self, app: AppBuilder) -> AppBuilder {
    ///         app.config_section("media") // `[media]` is now strict-config-safe
    ///         // … register routes, jobs, startup hooks, …
    ///     }
    /// }
    /// ```
    ///
    /// The declared roots are threaded into the default config loader
    /// ([`TomlEnvConfigLoader`](crate::config::TomlEnvConfigLoader)); a fully
    /// custom loader installed via
    /// [`with_config_loader`](AppBuilder::with_config_loader) owns its own
    /// strict-config handling and is unaffected.
    ///
    /// Future: an optional eager per-section validation hook (handing each
    /// plugin its raw `[root]` table at boot to validate uniformly) could be
    /// layered on top of this registry later. It is deliberately deferred — the
    /// media plugin already fail-fast-validates its own `[media]` config in its
    /// startup hook, and an eager hook adds callback-storage and error-surface
    /// plumbing this declarative seam does not need.
    #[must_use]
    pub fn config_section(mut self, root: impl Into<String>) -> Self {
        self.plugin_config_roots.insert(root.into());
        self
    }

    /// Return `true` if the given top-level config root has been declared as a
    /// plugin config section via [`config_section`](AppBuilder::config_section).
    ///
    /// Mirrors [`has_plugin`](AppBuilder::has_plugin); useful for tests and
    /// builder introspection.
    #[must_use]
    pub fn has_config_section(&self, root: &str) -> bool {
        self.plugin_config_roots.contains(root)
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
    ///
    #[cfg(feature = "db")]
    #[must_use]
    pub fn migrations(mut self, migrations: migrate::EmbeddedMigrations) -> Self {
        self.migrations.push(("app", migrations));
        self
    }

    /// Register embedded Diesel migrations owned by a plugin or other
    /// third-party integration, distinct from the app's own
    /// [`Self::migrations`].
    ///
    /// Functionally identical to [`Self::migrations`] — the set is applied at
    /// the same startup / one-shot points, subject to the same dev/prod
    /// auto-apply policy — but tagged with `name` (e.g.
    /// `"autumn-admin-plugin"`) rather than the generic `"app"` label
    /// [`Self::migrations`] uses.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// autumn_web::app()
    ///     .plugin_migrations("autumn-admin-plugin", autumn_admin_plugin::MIGRATIONS)
    ///     .migrations(MIGRATIONS)
    ///     .run()
    ///     .await;
    /// ```
    ///
    /// # Version collisions are resolved automatically, never rejected
    ///
    /// Diesel's `__diesel_schema_migrations` table is keyed by **version
    /// alone** — it has no notion of which registered source (the
    /// framework, a plugin, the app's own `migrations/`) recorded a
    /// version. Nothing coordinates timestamps across a plugin, the
    /// framework, and an app, so it is entirely normal for two
    /// independently authored migrations to reuse the same version by
    /// coincidence — e.g. both picking an all-zero placeholder for their
    /// first migration. Applied naively, whichever set's apply runs first
    /// would "win" the version, and every other set's same-versioned
    /// migration would be skipped forever as "already applied" even though
    /// its `up.sql` never actually ran.
    ///
    /// Rather than reject this — which would leave an app unable to use a
    /// plugin at all until someone renames a migration in a dependency they
    /// may not control — the framework detects the collision at apply time
    /// (across every registered set, including ones the framework itself
    /// folds in) and transparently tracks one of the colliding migrations
    /// under a distinguishing substitute version, so **both** migrations
    /// still apply. This is logged at `INFO` so it is visible, not silent.
    /// A version reused under the exact same full migration name (e.g. a
    /// shard-required set folded verbatim into another bundle too) is the
    /// separate, intentional, harmless case and is left untouched.
    ///
    /// Which of two colliding migrations keeps the plain version is decided
    /// by a fixed rule — the lexicographically-first full migration name —
    /// derived purely from the migrations' own content, **not** from
    /// registration order. So reordering `.migrations()`/`.plugin_migrations()`
    /// calls, or adding a new plugin, never changes an already-settled
    /// assignment for a collision that existed before.
    ///
    /// One case this cannot make safe on its own: introducing a **new**
    /// colliding source against a database that has **already** applied one
    /// side of the collision under its plain version from an *earlier*
    /// deploy (before the new source ever existed). Diesel's tracking table
    /// records only the bare version string, not which migration produced
    /// it, so there is no way to recover that history after the fact — the
    /// fixed rule above has no way to know a version it would assign to the
    /// new source is actually already claimed, in the real database, by the
    /// older one. If you introduce a plugin whose migrations might collide
    /// with an already-deployed app, verify manually before rolling out
    /// (e.g. confirm the plugin's expected tables don't already exist under
    /// a different name) rather than relying on this to resolve it for you.
    ///
    /// A second, narrower residual gap: `autumn migrate status` / `autumn
    /// migrate down` (the CLI's user-migration status and rollback commands)
    /// and the migration checksum/drift-detection system (`autumn migrate
    /// record-checksums`'s baseline and its later validation) all resolve
    /// applied versions against the app's own `migrations/` directory only —
    /// none of them have visibility into which plugins were registered at
    /// runtime. If your own app's migration is the one that loses a
    /// collision and gets tracked under a substitute version: `migrate
    /// status`/`migrate down` cannot currently resolve or revert it by name
    /// (reverting it needs manual intervention — inspect
    /// `__diesel_schema_migrations` directly); and checksum baselining/drift
    /// detection cannot see it either, so a later edit to that migration's
    /// `up.sql` will not trigger the drift guard. A plugin's own migrations
    /// are unaffected by either gap, since neither system touches plugin
    /// migrations either way. Fixing this needs the full registered
    /// migration set (not just the app's own directory) threaded through to
    /// these CLI-facing functions — out of scope for the auto-resolution
    /// added here.
    ///
    /// A third, even narrower edge case: an already-applied migration's
    /// substitute version is recomputed fresh on every startup from the
    /// CURRENTLY registered sources, reserved only against versions those
    /// sources currently claim. If a later release adds an entirely new
    /// migration whose own raw version happens to exactly equal an
    /// already-applied substitute (astronomically unlikely in practice,
    /// since a substitute is a source-name hash suffix no ordinary migration
    /// timestamp would organically collide with), the already-applied
    /// migration's substitute would be reassigned to free up that version —
    /// changing a previously-settled collision's tracked identity. This is
    /// accepted as a residual risk rather than solved by, say, persisting
    /// substitute assignments to the database, which the framework does not
    /// otherwise need to do.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn plugin_migrations(
        mut self,
        name: &'static str,
        migrations: migrate::EmbeddedMigrations,
    ) -> Self {
        self.migrations.push((name, migrations));
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
        // Remember the binary this process was started from, before a deploy
        // can replace the file underneath it: an in-place upgrade (#1674) execs
        // that path, and `/proc/self/exe` reports "… (deleted)" once the file
        // has been swapped.
        crate::upgrade::record_startup_exe();

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

        // ── OpenAPI spec dump mode ─────────────────────────────────────
        // When AUTUMN_DUMP_OPENAPI=1, print the generated OpenAPI document
        // and exit. Triggered by `autumn openapi export`, which needs the
        // contract without booting the server or connecting to a database.
        // The guard is deliberately outside the feature gate: a binary built
        // without `openapi` must report that on the dump protocol rather than
        // ignore the request and start serving.
        if is_dump_openapi_mode() {
            #[cfg(feature = "openapi")]
            {
                self.run_dump_openapi_mode().await;
                return;
            }
            #[cfg(not(feature = "openapi"))]
            {
                eprintln!(
                    "{marker}{reason}",
                    marker = crate::openapi::OPENAPI_UNAVAILABLE_MARKER,
                    reason = crate::openapi::OPENAPI_UNAVAILABLE_FEATURE,
                );
                std::process::exit(2);
            }
        }

        // ── Cache-coherence manifest dump mode ─────────────────────────
        // When AUTUMN_DUMP_CACHE_COHERENCE=1, print the cache-coherence
        // manifest (#1716) and exit. Triggered by `autumn cache audit`, which
        // needs the whole binary's registrations — every `#[cached]` read and
        // every `#[repository]` write, across the app AND its plugins — and
        // link-time `inventory` collection is the only place they all exist
        // together. Runs before any database or port is touched.
        if crate::cache::coherence::is_dump_mode() {
            crate::cache::coherence::print_manifest_dump(&crate::cache::coherence::audit());
            return;
        }

        // ── Data-flow manifest dump mode ───────────────────────────────
        // When AUTUMN_DUMP_DATA_FLOW=1, print the classified-data flow manifest
        // (#1654) and exit. Triggered by `autumn data-flow`, which needs the
        // whole binary's registrations -- every `#[classified]` column and every
        // declared declassification boundary, across the app AND its plugins --
        // and link-time `inventory` collection is the only place they all exist
        // together. Runs before any database or port is touched.
        if crate::classify::manifest::is_dump_mode() {
            crate::classify::manifest::print_manifest_dump(&crate::classify::manifest::audit());
            return;
        }

        // ── Agent-authority manifest dump mode ─────────────────────────
        // With AUTUMN_DUMP_AGENT_AUTHORITY=1, print the agent-authority manifest
        // (#1691) and exit. `autumn agents manifest` triggers this because it
        // needs the whole binary's registrations — every `#[agent_operable]`
        // action and every `authority_grant!`, across the app and its plugins —
        // joined against this app's route table: which actions an agent can reach
        // is a fact about the mounted routes, not the annotations alone. Runs
        // before any database or port is touched.
        if crate::agent_authority::manifest::is_dump_mode() {
            self.run_dump_agent_authority_mode();
            return;
        }

        // ── Architecture-graph dump mode ───────────────────────────────
        // When AUTUMN_DUMP_GRAPH=1, print the application architecture graph
        // (#1747) and exit. Triggered by `autumn graph`, which needs the whole
        // binary's registrations -- every `#[route]`, `#[model]`,
        // `#[repository]` and `#[job]`, across the app AND its plugins --
        // joined against this app's mounted route table, because a route's
        // served path and resolved auth posture are facts about the mount and
        // not about the annotation alone. Runs before any database or port is
        // touched.
        if crate::graph::manifest::is_dump_mode() {
            self.run_dump_graph_mode();
            return;
        }

        // ── Jobs manifest dump mode ────────────────────────────────────
        // When AUTUMN_DUMP_JOBS=1, print the effective drained-queue manifest
        // (TOML `queues = [...]`) and exit. Triggered by `autumn jobs manifest`
        // so a topology-aware `autumn doctor` sees exactly what the runtime
        // drains without booting the server or connecting to a database.
        if is_dump_jobs_mode() {
            self.run_dump_jobs_mode().await;
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

        // ── Migrate one-shot mode ──────────────────────────────────────
        // With AUTUMN_MIGRATE=1, apply pending embedded migrations to the
        // configured databases and exit; never start the HTTP server or bind a
        // port. `autumn deploy`'s redeploy cutover runs migrations before
        // flipping traffic (#1607), so a non-zero exit aborts the deploy with the
        // old release still serving (AC-3). Unlike startup auto-migration this
        // applies on every profile, because the deploy invokes it explicitly.
        if is_migrate_only_mode() {
            self.run_migrate_only_mode().await;
            return;
        }

        // ── Retention dry-run mode ──────────────────────────────────────
        // When AUTUMN_RETENTION_DRY_RUN=1, count (never delete) the rows every
        // registered `#[repository(..., retention(...))]` policy would sweep,
        // print the report as JSON, and exit — never starting the HTTP server.
        // Triggered by `autumn retention --dry-run` (issue #1342).
        if is_retention_dry_run_mode() {
            self.run_retention_dry_run_mode().await;
            return;
        }

        // ── Framework data-retention mode ───────────────────────────────
        // With AUTUMN_DB_RETENTION=report|purge, report or enforce the unified
        // `[retention]` policy over every framework-owned dataset and exit,
        // never starting the HTTP server. Triggered by `autumn db retention`
        // (#1605). It runs inside the app rather than the standalone CLI, so the
        // report reflects the app's own resolved config, GDPR legal-hold
        // registrations, and audit sinks — the inputs the scheduled sweep uses.
        if let Some(mode) = framework_retention_mode_from_env() {
            self.run_framework_retention_mode(mode).await;
            return;
        }

        // ── Capsule replay mode ────────────────────────────────────────
        // When AUTUMN_REPLAY_CAPSULE=<path> is set, rebuild this application
        // offline, drive the request the capsule recorded through it, print the
        // verdict and EXIT — never start the HTTP server, never open a socket
        // (issue #1598). Triggered by `autumn replay <capsule>`.
        if is_replay_mode()
            && let Some(capsule_path) = replay_capsule_from_env()
        {
            #[cfg(feature = "reporting")]
            {
                self.run_replay_mode(capsule_path).await;
                return;
            }
            // Capsules are a `reporting`-feature surface; without it there is
            // nothing to load the document with. Refuse (exit 2) rather than
            // silently booting a server the operator did not ask for.
            #[cfg(not(feature = "reporting"))]
            {
                eprintln!(
                    "REFUSED  {capsule_path}\n  this binary was built without the `reporting` \
                     feature, which failure capsules require"
                );
                std::process::exit(2);
            }
        }

        // Register the in-place upgrade signal (#1674) before the long boot —
        // config, database, migrations — rather than when the watcher task
        // first runs. Until a handler is installed, `SIGUSR2`'s default
        // disposition is to *terminate* the process, so a deploy script that
        // signals a process that is still booting would kill it.
        #[cfg(unix)]
        let upgrade_signal =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2()) {
                Ok(signal) => Some(signal),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "could not install the SIGUSR2 handler; in-place upgrade is unavailable"
                    );
                    None
                }
            };

        let Self {
            routes,
            api_versions,
            route_sources: _,
            current_plugin: _,
            mut tasks,
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
            plugin_contracts: _,
            plugin_config_roots,
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
            alert_channels,
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
            mail_suppression_store,
            #[cfg(feature = "mail")]
            mount_unsubscribe_endpoint,
            #[cfg(feature = "mail")]
            mail_previews,
            #[cfg(feature = "maud")]
            story_gallery,
            declared_routes,
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

        // #1342: every `#[repository(..., retention(...))]` policy compiled
        // into this binary auto-registers here — no `tasks![...]` entry
        // required. `inventory`-collected, so this is a no-op allocation
        // when no model declares a policy.
        #[cfg(feature = "db")]
        tasks.extend(crate::retention::collect_retention_tasks());

        // `collect_retention_tasks()` catches collisions only among
        // retention-generated names; it cannot see hand-declared `tasks![...]`
        // entries merged in above. An operator's `#[scheduled]` task sharing a
        // name with a generated `retention-sweep-<table>` task, or with another
        // hand-declared task, would silently spawn two competing scheduler loops.
        // Validate the fully merged list now that every name is visible, rather
        // than asking operators to avoid the generated namespace by convention.
        if let Err(error) = crate::task::validate_unique_scheduled_task_names(&tasks) {
            panic!("{error}");
        }

        let all_routes = routes;

        // 1 & 2. Load configuration and initialize logging/telemetry
        let (mut config, telemetry_guard) = load_config_and_telemetry(
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
        )
        .await;

        // #1605: the unified framework-owned data-retention sweep. Registered
        // here rather than with the `#[repository(..., retention(...))]` policies
        // above, because it is config-driven and the config loads only now.
        // `framework_retention_task` returns `None` unless at least one
        // `[retention]` window is set, so an app that never mentions the section
        // gets no extra scheduler loop.
        if let Some(retention_task) =
            crate::data_retention::framework_retention_task(&config.retention)
        {
            tasks.push(retention_task);
            // Re-validate: the merged-name check above ran before this task
            // existed, and an app is free to declare a `#[scheduled]` fn
            // named `autumn-retention-sweep`, which would otherwise spawn two
            // loops competing for one coordination lock.
            if let Err(error) = crate::task::validate_unique_scheduled_task_names(&tasks) {
                panic!("{error}");
            }
        }

        // The process role selects which slice of the runtime this replica runs.
        // A split role (web/worker) needs a durable jobs backend both processes
        // share. Anything else — the in-process `local` queue, a typo, a blank
        // value — falls through to the per-process local runtime: the web replica
        // enqueues into an in-memory queue no worker can drain, and a worker
        // replica's queue starts empty. Reject it here, before any boot work,
        // rather than in `validate()`, so the doctor can still load the config.
        // A combined role is always fine.
        let role = config.role;
        if crate::config::split_role_requires_durable_backend(role, &config.jobs.backend) {
            tracing::error!(
                role = role.as_str(),
                jobs_backend = %config.jobs.backend,
                "process role '{}' requires a durable jobs backend: backend '{}' is not \
                 a recognized durable backend and falls through to the in-process 'local' \
                 runtime, which cannot be shared across a split web/worker topology. \
                 Set jobs.backend = \"postgres\" or \"redis\", or run the combined role.",
                role.as_str(),
                config.jobs.backend,
            );
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        }

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

        // 4f. Provision the configured BlobStore before `setup_database`.
        // `LocalBlobStore::new` does real IO (it creates and canonicalizes the
        // root) and the storage code may `process::exit(1)` on failure — an
        // unwritable root, or `storage.backend = "s3"` with no plugin. Running it
        // before migrations keeps a doomed boot from mutating the DB schema
        // first. A store installed via `.with_blob_store(...)` bypasses
        // config-driven instantiation entirely: no IO, no fail-fast.
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
        //
        // With `[failure_capture] enabled = true` the pool is built through the
        // recording factory, so a failing request's database traffic is captured
        // at the wire (#1598). An app with its own `DatabasePoolProvider` keeps
        // it, and DB capture stands down.
        //
        // This wraps the control topology only. `[[database.shards]]` pools are
        // built by `create_shard_set` below, outside `setup_database`, and are
        // not recorded in this slice. So `Db::checkout` notes the gap and marks
        // the capsule truncated for any request that checks out a shard
        // connection (`capsule::record_db::note_shard_capture_gap`), and
        // `maybe_capture_pool_provider` warns at boot when both are configured.
        #[cfg(all(feature = "db", feature = "reporting", not(feature = "sqlite")))]
        let pool_provider_factory =
            crate::capsule::record_db::maybe_capture_pool_provider(pool_provider_factory, &config);
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

        // Tee clock reads into the capsule of whatever request took them, so a
        // replayed handler sees the same `now()` sequence the failure did.
        // Mirrored by `TestApp::build` so test apps exercise the same wiring.
        #[cfg(feature = "reporting")]
        if config.failure_capture.enabled {
            let recording =
                std::sync::Arc::new(crate::capsule::RecordingClock::new(state.clock_arc()))
                    as std::sync::Arc<dyn crate::time::ClockSource>;
            state = state.with_clock(recording);
        }
        // Same for the entropy source (#1634): a handler that mints a session
        // id, a token or a job id must mint the *recorded* one on replay, or
        // the identifier in the capsule's SQL binds will not be the one the
        // replayed code produced.
        #[cfg(feature = "reporting")]
        if config.failure_capture.enabled {
            let recording =
                std::sync::Arc::new(crate::capsule::RecordingEntropy::new(state.entropy_arc()))
                    as std::sync::Arc<dyn crate::entropy::Entropy>;
            state = state.with_entropy(recording);
        }

        // Wire the in-memory log capture buffer from the telemetry guard into the
        // app state so the `/actuator/logfile` endpoint can serve it.
        if let Some(buf) = telemetry_guard.log_buffer.clone() {
            state.insert_extension(buf);
        }
        // Wire the live-subscriber reload handle into the loggers actuator so
        // `PUT /actuator/loggers/{name}` affects the running subscriber, not
        // just an in-memory map (issue #1044).
        if let Some(handle) = telemetry_guard.filter_reload.clone() {
            state.log_levels().attach_reload_handle(handle);
        }

        // Build MaintenanceState, load the flag synchronously, insert it as an
        // extension, and start the background poller.
        //
        // #1621: the flag path comes from `maintenance::resolve_flag_file_path()`,
        // not the bare cwd-relative const. A deploy-managed slot unit runs with
        // `WorkingDirectory={release_dir}`, a fresh dir every release, so the
        // legacy path made a cutover orphan the flag and silently un-maintain the
        // host. The resolver honours `AUTUMN_MAINTENANCE_FLAG_FILE`, stamped by
        // `autumn deploy` at the per-app `shared/` dir that survives cutovers, and
        // falls back to the legacy path when unset, so a non-deploy-managed app is
        // unaffected. This boot load and the 500 ms poller below use the same
        // resolver, so they cannot disagree.
        let maintenance_state = crate::maintenance::MaintenanceState::new();
        let flag_path = crate::maintenance::resolve_flag_file_path();
        if let Ok(Some(cfg)) = crate::maintenance::MaintenanceState::load_from_file(&flag_path) {
            maintenance_state.enable(cfg);
        }
        state.insert_extension(maintenance_state.clone());

        let poller_state = maintenance_state.clone();
        tokio::spawn(async move {
            let path = crate::maintenance::resolve_flag_file_path();
            let interval = std::time::Duration::from_millis(500);
            loop {
                let poll_path = path.clone();
                let load_res = tokio::task::spawn_blocking(move || {
                    crate::maintenance::MaintenanceState::load_from_file(&poll_path)
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

        // Continuous SQLite replication (#1628). Resolved here, next to the other
        // indicator registrations, so lag and verification are baked into
        // `/actuator/health` — and so an indicator that stays non-healthy past
        // the grace period is escalated by the existing #1610 alerter with no
        // bespoke alert condition of its own. The destination is built on a
        // BLOCKING thread: its HTTP client must never be constructed inside the
        // async runtime. The loop itself is spawned at bind time below.
        #[cfg(feature = "db")]
        let mut replication_worker: Option<crate::replication::Replicator> = None;
        #[cfg(feature = "db")]
        if let Some(replication_config) = config.replication.clone() {
            let database_url = config
                .database
                .primary_url
                .clone()
                .or_else(|| config.database.url.clone())
                .unwrap_or_default();
            let profile = config.profile.clone().unwrap_or_else(|| "dev".to_owned());
            // Only a genuinely S3-backed app has a blob-storage bucket to clash
            // with; a leftover `[storage.s3]` bucket on the local backend is
            // inert and must not trip the distinct-destination guard (the same
            // rule #1619's offsite upload applies).
            #[cfg(feature = "storage")]
            let storage_destination = (config.storage.backend
                == crate::storage::StorageBackend::S3)
                .then(|| {
                    config
                        .storage
                        .s3
                        .bucket
                        .clone()
                        .map(|bucket| (bucket, config.storage.s3.endpoint.clone()))
                })
                .flatten();
            #[cfg(not(feature = "storage"))]
            let storage_destination: Option<(String, Option<String>)> = None;
            // The injected clock, not the wall clock: every artifact the
            // replicator stamps, and the health indicator's startup grace, are
            // read from it, so a test that freezes time moves them all (#1797).
            let clock = state.clock_arc();

            let built = tokio::task::spawn_blocking(move || {
                crate::replication::build(
                    &replication_config,
                    &database_url,
                    &profile,
                    storage_destination.as_ref().map(|(bucket, endpoint)| {
                        crate::replication::StorageDestination {
                            bucket,
                            endpoint: endpoint.as_deref(),
                        }
                    }),
                    clock,
                )
            })
            .await;

            match built {
                Ok(Ok(runtime)) => {
                    tracing::info!(
                        database = %runtime.settings.database_path.display(),
                        destination = %runtime.status.snapshot().destination,
                        prefix = %runtime.settings.root,
                        sync_interval_secs = runtime.settings.sync_interval.as_secs(),
                        "continuous SQLite replication is enabled"
                    );
                    if let Err(e) = state.health_indicator_registry.register(
                        crate::replication::INDICATOR_NAME,
                        crate::actuator::IndicatorGroup::HealthOnly,
                        std::sync::Arc::clone(&runtime.indicator)
                            as std::sync::Arc<dyn crate::actuator::HealthIndicator>,
                    ) {
                        tracing::warn!("{e}");
                    }
                    replication_worker = Some(runtime.replicator);
                }
                Ok(Err(crate::replication::SetupError::Disabled)) => {}
                // A misconfigured durability story must not boot silently
                // half-working: the operator asked for replication and is not
                // getting it, which is exactly the situation this feature exists
                // to prevent.
                Ok(Err(e)) => {
                    tracing::error!("Continuous SQLite replication could not start: {e}");
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(1);
                }
                Err(e) => {
                    tracing::error!("Continuous SQLite replication setup panicked: {e}");
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(1);
                }
            }
        }

        // When ACME is configured, register a `HealthOnly` indicator backed by a
        // shared status the renewal task writes. Built here (before the router)
        // so it is baked into `/actuator/health`; the same `AcmeStatus` handle is
        // reused by the renewal task spawned at bind time below.
        #[cfg(feature = "acme")]
        let acme_status: Option<crate::acme::renewal::AcmeStatus> =
            if let Some(acme_cfg) = config.server.tls.as_ref().and_then(|t| t.acme.as_ref()) {
                let status = crate::acme::renewal::AcmeStatus::new();
                let indicator = std::sync::Arc::new(
                    crate::acme::renewal::AcmeHealthIndicator::new(
                        status.clone(),
                        acme_cfg.renew_before_days,
                    )
                    // Which challenge is in play (and, for DNS-01, which provider)
                    // is the first thing an operator needs when issuance is failing
                    // — and is safe to publish: it names no credential (#1620).
                    .with_dns_provider(acme_cfg.dns.as_ref().map(|dns| dns.provider.as_str())),
                );
                if let Err(e) = state.health_indicator_registry.register(
                    "acme",
                    crate::actuator::IndicatorGroup::HealthOnly,
                    indicator,
                ) {
                    tracing::warn!("{e}");
                }
                Some(status)
            } else {
                None
            };

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

        // Capture a clone of the registered reporter chain for the ACME renewal
        // task (spawned below) so each renewal failure reaches the same
        // Sentry/etc. sinks a request-path 5xx would. Empty is fine — failures
        // still log via `tracing` inside the loop.
        #[cfg(all(feature = "acme", feature = "reporting"))]
        let acme_reporters = error_reporters.clone();

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
        if let Some(handle) = mail_suppression_store {
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
        #[cfg(feature = "maud")]
        install_story_registry(&state, story_gallery);
        // Operator alerts: build the built-in mail/webhook channels from
        // `[alerts]` config, combine with any builder-registered channels, and
        // start the background evaluation loop. No-op when nothing is
        // configured. Installed after the mailer so the mail channel can bind
        // to the live `Mailer` extension.
        crate::alerts::install_from_config(&state, &config.alerts, alert_channels);
        // An MCP endpoint with no audit sink still serves tools; what it does
        // not do is leave a record that an agent called one. That is a property
        // of the deployment rather than of any grant, so it is said once, here,
        // where both are known -- and it is also carried in the agent-authority
        // manifest's `audit.sink_configured` (#1691 R9).
        #[cfg(feature = "mcp")]
        if mcp.is_some()
            && !audit_logger
                .as_ref()
                .is_some_and(|logger| logger.is_enabled())
        {
            tracing::warn!(
                target: "autumn.agent",
                "MCP is mounted with no audit sink installed: agent tool calls will be traced \
                 but not recorded. Install one with `AppBuilder::with_audit_sink(..)`. \
                 See docs/guide/agent-authority.md"
            );
        }
        if let Some(logger) = audit_logger {
            state.insert_extension::<crate::audit::AuditLogger>((*logger).clone());
        }
        #[cfg(feature = "i18n")]
        let custom_layers =
            install_i18n_bundle_layer(custom_layers, &state, i18n_bundle, &config.i18n);

        // Install the preflighted blob store on the freshly-built
        // AppState, and remember the serving router so it gets merged
        // into the user's router below.
        #[cfg(feature = "storage")]
        let storage_router = storage_bootstrap.and_then(|b| b.install(&state));
        install_webhook_registry(&state, &config);
        run_state_initializers(state_initializers, &state);
        // A live-state block that could not be installed is a refusal to start,
        // not a silent fallback: the previous build is still serving and still
        // holds the only copy of that state (#1674). Checked here, in async
        // context, so a managed-Postgres child is stopped rather than orphaned.
        if let Some(failure) = state.extension::<crate::upgrade::LiveStateInstallFailure>() {
            tracing::error!("refusing to start: {}", failure.0);
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        }
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

        // Static routes are pre-rendered by requesting their single,
        // unprefixed path — never locale-aware — so they must stay excluded
        // from locale-prefix routing even when it's enabled (issue #1251).
        // Must run before both the sitemap generation below and the router
        // build further down, so the two agree on which paths are excluded.
        exclude_static_routes_from_locale_prefix(&mut config, &static_metas);
        // `state` already holds an `AutumnConfig` snapshot cloned inside
        // `build_state` above — captured BEFORE this mutation. Refresh it, or
        // `tenancy_middleware` (which reads config via
        // `state.extension::<AutumnConfig>()`) would see a stale copy missing
        // these auto-excluded static routes and could misjudge whether a
        // `/{locale}`-look-alike path was ever actually locale-prefixed
        // (Codex review).
        state.insert_extension(config.clone());

        // Register SEO routes (/robots.txt and /sitemap.xml) when any SEO
        // configuration is present or dynamic sources are registered.
        if !seo_sources.is_empty() || crate::seo::has_seo_config(&config.seo) {
            let seo_cfg = &config.seo;
            let raw_profile = config.profile.as_deref().unwrap_or("dev");
            let profile = crate::seo::effective_seo_profile(raw_profile, seo_cfg.robots.allow_all);
            // A static route that declared `seo(robots = "noindex")` must not
            // be advertised in sitemap.xml — otherwise the app tells crawlers
            // "here is this URL" and "do not index it" at the same time (#1182).
            let static_paths: Vec<&str> = static_metas
                .iter()
                .filter(|m| !crate::seo::defaults_exclude_from_sitemap(m.seo))
                .map(|m| m.path)
                .collect();
            let (robots_body, sitemap_body) = crate::seo::assemble_seo_bodies(
                profile,
                seo_cfg.base_url.as_deref(),
                seo_cfg.robots.sitemap_url.as_deref(),
                &seo_cfg.robots.additional_rules,
                &seo_sources,
                &static_paths,
                sitemap_locale_config(&config),
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
                })
                // Declared plugin routes belong in this check for the same reason
                // as the others. A sandboxed manifest may take `/robots.txt` as
                // its prefix — a `.` is legal inside a prefix segment — and its
                // routes nest after this router merges, so the two would overlap
                // and axum would panic at startup. The declared-route preflight
                // cannot catch it either: it compares against a claim set built
                // from config alone, and SEO also mounts when a source is
                // registered in code. Yielding matches what this site already does
                // for a custom `#[static_get("/robots.txt")]`, and the operator saw
                // the prefix on the consent screen before installing it.
                || declared_routes.iter().any(|r| {
                    // Only a GET can clash with the generated GETs: a declared
                    // POST or HEAD merges cleanly into the same `MethodRouter`
                    // (verified against axum 0.8.9, the same finding
                    // `reject_declared_framework_collisions` is written on), and
                    // a `WS` upgrade mounts as a GET. Yielding to a disjoint
                    // verb would hand an untrusted plugin a way to suppress
                    // robots.txt and sitemap.xml without even serving them.
                    (r.method.eq_ignore_ascii_case("GET")
                        || r.method.eq_ignore_ascii_case("WS"))
                        && is_seo_path(&r.path)
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

        // Worker role does not serve user routes: build a probe-only router that
        // exposes just the framework liveness/readiness probes and the actuator,
        // so orchestrators can supervise the process and `/actuator/jobs` works.
        // Web and combined roles build the full application router. All the
        // route/router-context inputs assembled above are simply dropped in the
        // worker branch.
        // Publish the architecture graph this process serves (#1747) before the
        // router is built, so `/actuator/graph` answers from the first request
        // rather than after some later warm-up. This is the *serving* path —
        // the static-build and capsule-replay paths publish their own below.
        // `graph_installed_before_every_router_build` pins all three, because a
        // graph installed on only some of them is an endpoint that answers 503
        // in production while every unit test passes.
        //
        // A worker serves the probe-only router: it mounts no application route
        // at all, and drops every raw router the builder collected. Publishing
        // the full route table there would have `/actuator/graph` — which a
        // worker can expose — describe endpoints this process does not serve
        // (Codex round 4). The declared elements are still nodes, because they
        // are still compiled in; they simply report `mounted: false`, and the
        // completeness section names them, which is the honest answer to "what
        // does this process serve".
        let (graph_mounted, graph_opaque) = if role.serves_http() {
            (
                graph_mounted_routes(&all_routes, &scoped_groups, &declared_routes, &config),
                omitted_router_count(
                    merge_routers.len(),
                    nest_routers.iter().map(|(prefix, _)| prefix.as_str()),
                    &declared_routes,
                ),
            )
        } else {
            (Vec::new(), 0)
        };
        crate::graph::install(crate::graph::manifest::audit(&graph_mounted, graph_opaque));

        let router_build = if role.serves_http() {
            crate::router::try_build_router_with_static_inner(
                all_routes,
                &config,
                state.clone(),
                dist_ref,
                crate::router::RouterContext {
                    exception_filters,
                    scoped_groups,
                    merge_routers,
                    nest_routers,
                    // The sandboxed-plugin manifests (and any other declared plugin
                    // routes) this builder collected. Handing them to the router build is
                    // what lets the duplicate-route preflight see inside an otherwise
                    // opaque `nest` mount and refuse a collision instead of panicking.
                    declared_routes,
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
        } else {
            crate::router::try_build_probe_only_router(&config, state.clone())
        };
        let router = router_build.unwrap_or_else(|error| {
            tracing::error!(error = %error, "Failed to build router");
            exit_stop_managed_pg();
            std::process::exit(1);
        });

        // 7. Bind and initialize pre-serve runtime dependencies. Start listening
        // before the startup hooks finish, so `/startup` can report honest
        // progress.
        // Bind the configured transport. A `server.unix_socket` path selects a
        // Unix domain socket (local daemon mode); otherwise bind TCP on
        // `host:port`. `bound_desc` is the log description, and
        // `unix_socket_cleanup` is the socket to unlink on clean exit — axum does
        // not remove it — carried as `(path, dev, inode)` so cleanup can confirm
        // the file is still the one this process bound.
        // Load the `[push]` VAPID key once, before binding. A key that is present
        // but unusable — a typo, an env var that failed to interpolate, a
        // mismatched public/private pair — is a hard boot failure rather than a
        // quiet fallback to "push disabled". The failure this guards is an app
        // that starts cleanly, records subscriptions, and silently delivers
        // nothing (#1392). An app with no `[push]` block is unaffected.
        if let Err(e) = config.validate_push() {
            tracing::error!("Invalid [push] configuration: {e}");
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        }

        // Validate `[server.tls]` wiring before we bind anything, so a
        // misconfiguration is a clear pre-bind failure. Two cases fail fast:
        // (1) the section is present but this binary was built without the
        // `tls` feature — otherwise it would be silently ignored and the app
        // would serve plain HTTP on a port operators expect to be HTTPS;
        // (2) TLS is combined with a Unix socket, which the direct-HTTPS path
        // does not serve over (TLS terminates on `host:port`).
        if let Some(tls_cfg) = config.server.tls.as_ref() {
            // Reject an incoherent `[server.tls]` (both static + ACME, neither,
            // a half-set cert pair, an empty/wildcard ACME domain set, …) before
            // binding, with a named message. Pure config validation, so it runs
            // regardless of build features.
            if let Err(msg) = tls_cfg.validate() {
                tracing::error!("Invalid [server.tls] configuration: {msg}");
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop_async().await;
                std::process::exit(1);
            }
            #[cfg(not(feature = "tls"))]
            {
                tracing::error!(
                    "[server.tls] is configured but this binary was built without the `tls` \
                     feature; rebuild with `--features tls`, or remove [server.tls] to serve \
                     plain HTTP"
                );
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop_async().await;
                std::process::exit(1);
            }
            // `[server.tls.acme]` needs the `acme` feature; otherwise it would be
            // silently ignored and the app would serve a self-signed placeholder
            // (or fail) on a port operators expect to serve a real ACME cert.
            #[cfg(all(feature = "tls", not(feature = "acme")))]
            if tls_cfg.acme.is_some() {
                tracing::error!(
                    "[server.tls.acme] is configured but this binary was built without the \
                     `acme` feature; rebuild with `--features acme`, or configure a static \
                     cert_path/key_path instead"
                );
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop_async().await;
                std::process::exit(1);
            }
            #[cfg(feature = "tls")]
            if config.server.unix_socket.is_some() {
                tracing::error!(
                    "[server.tls] cannot be combined with server.unix_socket; direct TLS \
                     terminates on host:port. Unset one of them"
                );
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop_async().await;
                std::process::exit(1);
            }
        }

        // Root shutdown token for all background tasks. Created before the bind
        // block so the TLS listener's background acceptor task can take a child
        // token and stop cleanly on shutdown (issue #1603).
        let server_shutdown = tokio_util::sync::CancellationToken::new();

        // Carries the cert/key reload wiring from the TLS bind path to the
        // background reload task spawned once `server_shutdown` exists.
        #[cfg(feature = "tls")]
        let mut tls_reload_state: Option<crate::tls::CertReloader> = None;

        // Carries the ACME challenge listener + renewal task wiring from the TLS
        // bind path to the sibling tasks spawned once `server_shutdown` exists.
        #[cfg(feature = "acme")]
        let mut acme_bind_state: Option<AcmeBindState> = None;

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
                // Bind under an owner-only umask so the socket is `0600` from the
                // start. A plain bind would briefly leave it group- or
                // other-connectable, and a later `chmod` cannot revoke a
                // connection already established in that window. This matters for
                // a user-configured `server.unix_socket` in a shared dir.
                // `umask` is process-wide, so serialize save/bind/restore: a
                // concurrent UDS bind in the same process could otherwise
                // interleave the pairs and either reopen that window or leave
                // `0177` set permanently. The guard is released before the
                // `.await` in the error arm below.
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
            // A successor that terminates TLS cannot take over a plaintext
            // listener: the socket it inherited is mid-conversation with HTTP
            // clients, and wrapping it in rustls fails every one of them while
            // both builds accept from the shared queue. Refuse the same way an
            // existing TLS or Unix listener is refused — before binding
            // anything, so the predecessor's wait ends on this process exiting.
            #[cfg(feature = "tls")]
            if config.server.tls.is_some() && crate::upgrade::handoff_requested() {
                tracing::error!(
                    "refusing to start: this build terminates TLS ([server.tls]) but was \
                     handed the previous build's plaintext listening socket. An in-place \
                     upgrade cannot change the transport — restart the process to apply it. \
                     The previous build keeps serving"
                );
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop_async().await;
                std::process::exit(1);
            }
            let configured_addr = format!("{}:{}", config.server.host, config.server.port);
            let listener = bind_or_adopt_tcp_listener(&configured_addr).await;
            // Report where this process is *actually* listening, not what was
            // asked for: a socket inherited from a predecessor (#1674) keeps
            // that process's port, and `server.port = 0` resolves to whichever
            // ephemeral port the kernel picked.
            let addr = listener
                .local_addr()
                .map_or(configured_addr, |bound| bound.to_string());
            // When `[server.tls]` is set (and the `tls` feature is built in),
            // wrap the just-bound TCP listener in a rustls acceptor so the same
            // host:port serves HTTPS. Fail fast on any cert/key problem — the
            // pre-bind guard already rejected a Unix-socket combination and a
            // feature-less build, so reaching here with `tls = Some` means the
            // feature is on.
            #[cfg(feature = "tls")]
            {
                if let Some(tls_cfg) = config.server.tls.as_ref() {
                    // ACME mode: build the resolver from a stored cert if present,
                    // else a self-signed placeholder so `:443` binds immediately;
                    // the renewal task swaps the real cert in once issued.
                    #[cfg(feature = "acme")]
                    if let Some(acme_cfg) = tls_cfg.acme.as_ref() {
                        let https_port = config.server.port;
                        match build_acme_tls_listener(
                            listener,
                            tls_cfg,
                            acme_cfg,
                            &config.credentials,
                            https_port,
                            acme_status.clone(),
                            server_shutdown.child_token(),
                        )
                        .await
                        {
                            Ok((tls_listener, bind_state)) => {
                                acme_bind_state = Some(bind_state);
                                (
                                    BoundListener::Tls(tls_listener),
                                    format!("https://{addr} (ACME)"),
                                    None,
                                )
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to configure [server.tls.acme]");
                                #[cfg(feature = "managed-pg")]
                                crate::managed_pg::emergency_stop_async().await;
                                std::process::exit(1);
                            }
                        }
                    } else {
                        match build_tls_listener(listener, tls_cfg, server_shutdown.child_token()) {
                            Ok((tls_listener, reload)) => {
                                tls_reload_state = Some(reload);
                                (
                                    BoundListener::Tls(tls_listener),
                                    format!("https://{addr}"),
                                    None,
                                )
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to configure [server.tls]");
                                #[cfg(feature = "managed-pg")]
                                crate::managed_pg::emergency_stop_async().await;
                                std::process::exit(1);
                            }
                        }
                    }
                    #[cfg(not(feature = "acme"))]
                    match build_tls_listener(listener, tls_cfg, server_shutdown.child_token()) {
                        Ok((tls_listener, reload)) => {
                            tls_reload_state = Some(reload);
                            (
                                BoundListener::Tls(tls_listener),
                                format!("https://{addr}"),
                                None,
                            )
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to configure [server.tls]");
                            #[cfg(feature = "managed-pg")]
                            crate::managed_pg::emergency_stop_async().await;
                            std::process::exit(1);
                        }
                    }
                } else {
                    (BoundListener::Tcp(listener), addr, None)
                }
            }
            #[cfg(not(feature = "tls"))]
            {
                (BoundListener::Tcp(listener), addr, None)
            }
        };

        let shutdown_timeout = config.server.shutdown_timeout_secs;
        let prestop_grace = config.server.prestop_grace_secs;

        if let Err(error) = initialize_job_runtime(
            jobs,
            &state,
            &server_shutdown,
            &config.jobs,
            role.runs_workers(),
        ) {
            tracing::error!(error = %error, "job runtime initialization failed");
            // Post-DB failure: `process::exit` skips `on_shutdown`, so stop any
            // managed Postgres before bailing.
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        }

        // Embedded cluster control plane (#1762). Mirrors the
        // `crate::alerts::install_from_config` precedent — a no-op when
        // `[cluster]` is disabled — but installed here rather than beside alerts
        // because it owns a listener and two loops. A cluster that cannot bind or
        // start is a hard boot failure: a node that silently never joins would
        // serve its own private view of a counter it claims is cluster-wide.
        //
        // Its token is deliberately not a child of `server_shutdown`. That token
        // fires at phase 5, when the listener stops accepting, while in-flight
        // requests still drain for up to `shutdown_timeout_secs`. A request served
        // during that drain can still increment a cluster counter, and with the
        // push loop already departed the increment would land in a document
        // nothing replicates and die with the process. The cluster is therefore
        // cancelled after the drain completes (see `cluster_shutdown.cancel()`
        // below), inside the same budget.
        let cluster_shutdown = tokio_util::sync::CancellationToken::new();
        if let Err(error) =
            crate::cluster::install_from_config(&state, &config.cluster, &cluster_shutdown)
        {
            tracing::error!(error = %error, "cluster installation failed");
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        }

        #[cfg(feature = "db")]
        {
            #[cfg(feature = "ws")]
            crate::repository_commit_hooks::set_global_channels(state.channels().clone());
        }

        // Draining durable after-commit hook rows is background execution, so gate
        // it on the process role exactly like the `#[job]` runtime above: a `web`
        // replica must not claim or execute hook rows, while `worker` and
        // `combined` replicas keep running it. The worker drains rows through a
        // Postgres queue (LISTEN/NOTIFY plus row-locked claiming). Under the
        // `sqlite` feature the runtime pool is a SQLite pool the Postgres worker
        // cannot drive, so the worker is not spawned.
        #[cfg(all(feature = "db", not(feature = "sqlite")))]
        if role.runs_workers()
            && let Some(pool) = state.pool().cloned()
        {
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
        // commit hooks into that shard's queue table; drain each one too — again
        // only on a role that runs workers, so a web replica leaves shard hook
        // rows for the worker tier.
        #[cfg(all(feature = "db", not(feature = "sqlite")))]
        if role.runs_workers()
            && let Some(shards) = state.shards()
        {
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
        // SQLite durable commit-hook worker (#1996 item 5): the runtime pool is a
        // single-node SQLite pool the Postgres queue worker cannot drive, so the
        // SQLite worker (BEGIN IMMEDIATE claim + in-process Notify kick + poll
        // fallback) is spawned instead — same `role.runs_workers()` gate and same
        // `server_shutdown.child_token()` graceful-drain wiring as the Postgres tier.
        #[cfg(all(feature = "db", feature = "sqlite"))]
        if role.runs_workers()
            && let Some(pool) = state.pool().cloned()
        {
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

        // The replication loop runs on a dedicated OS thread, not a
        // `spawn_blocking` task: it lives for the whole process and does blocking
        // file, SQLite, and HTTP work, which would pin a thread in tokio's
        // blocking pool — sized for short tasks — forever. It stops with the
        // server and ships one final time on the way out.
        //
        // Its token is deliberately not a child of `server_shutdown`, which fires
        // at phase 5 when the listener stops accepting. Requests keep draining
        // after that and still commit, so the loop's final tick must come later:
        // the token is cancelled explicitly below, once the drain finishes. Same
        // reasoning as the cluster's token, for the same class of dropped write.
        #[cfg(feature = "db")]
        let replication_shutdown = tokio_util::sync::CancellationToken::new();
        // Signals that `Replicator::run` has returned — i.e. the final flush is
        // done. A channel rather than a `JoinHandle`: waiting on a join means
        // blocking, and a blocking wait that times out cannot be cancelled, so a
        // stuck upload would hold the runtime open past the shutdown budget it
        // was supposed to be bounded by. Dropping this receiver abandons the
        // thread instead, which is what the budget expiring is supposed to mean.
        #[cfg(feature = "db")]
        let mut replication_done: Option<tokio::sync::oneshot::Receiver<()>> = None;
        #[cfg(feature = "db")]
        if let Some(replicator) = replication_worker.take() {
            let replication_shutdown = replication_shutdown.clone();
            let (finished, waiter) = tokio::sync::oneshot::channel();
            match std::thread::Builder::new()
                .name("autumn-sqlite-replication".to_owned())
                .spawn(move || {
                    replicator.run(&replication_shutdown);
                    // The receiver is gone when shutdown stopped waiting; the
                    // flush still happened, so there is nothing to report.
                    let _ = finished.send(());
                }) {
                Ok(_handle) => replication_done = Some(waiter),
                // Serving on without the replicator is the worst of both
                // worlds: the pool has already disabled auto-checkpointing for
                // it, so the -wal would grow until the disk filled, and the
                // operator would believe they were replicating. Fail loudly
                // instead.
                Err(e) => {
                    tracing::error!("Could not start the SQLite replication thread: {e}");
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(1);
                }
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

        // TLS certificate hot-reload: poll the cert/key file mtimes on an
        // interval and swap the served certificate in place on change (e.g.
        // after a `certbot`/ACME renewal), so a renewal is picked up WITHOUT a
        // restart. A child of `server_shutdown`, exactly like the presence
        // sweep above, so it stops cleanly on shutdown. A failed reload logs an
        // error and keeps serving the previously loaded certificate — a bad
        // renewal never breaks the listener.
        #[cfg(feature = "tls")]
        if let Some(reload) = tls_reload_state.take() {
            let reload_shutdown = server_shutdown.child_token();
            tokio::spawn(async move {
                reload.run(reload_shutdown).await;
            });
        }

        // ACME (issue #1608): bind the `:80` HTTP-01 challenge + HTTP→HTTPS
        // redirect listener and spawn the renewal loop, each a child of
        // `server_shutdown` so they tear down with the main server. The renewal
        // loop runs on every replica (a pure `web` replica must renew its own
        // cert) and leader-elects through the scheduler coordinator so only one
        // replica orders per certificate.
        #[cfg(feature = "acme")]
        if let Some(bind_state) = acme_bind_state.take() {
            let AcmeBindState {
                mut renewal_task,
                tokens,
                http_challenge_port,
                https_port,
                dns01,
            } = bind_state;

            // The `:80` challenge/redirect listener, bound dual-stack so the CA
            // can validate HTTP-01 over IPv4 and IPv6 — an AAAA-only host is
            // otherwise unreachable on `:80`. Preferred: one `[::]` socket with
            // IPV6_V6ONLY=false; on a platform that refuses it, a separate IPv4
            // and IPv6 pair, each served below. A bind error under HTTP-01 is
            // fail-fast: `:80` needs CAP_NET_BIND_SERVICE and validation cannot
            // succeed without it.
            //
            // Under DNS-01 it is only a warning. The CA never connects here —
            // domain control is proved by a TXT record — so the listener is just
            // the HTTP→HTTPS redirect. Exiting would kill the deployment #1620
            // exists to serve: a container without CAP_NET_BIND_SERVICE using
            // DNS-01 because `:80` is unavailable. `autumn doctor` grades this the
            // same way, and the runtime must not refuse a config it passes.
            let challenge_listeners =
                match crate::acme::challenge::bind_challenge_listeners(http_challenge_port).await {
                    Ok(listeners) => listeners,
                    Err(e) if dns01 => {
                        tracing::warn!(
                            port = http_challenge_port,
                            error = %e,
                            "Could not bind the ACME challenge/redirect listener. DNS-01 issuance \
                             does not need it, so startup continues — but visitors who type \
                             http:// will not be redirected to HTTPS. Grant \
                             CAP_NET_BIND_SERVICE, or set [server.tls.acme] http_challenge_port \
                             to a port this process may bind"
                        );
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::error!(
                            port = http_challenge_port,
                            "Failed to bind the ACME HTTP-01 challenge listener: {e}. Port \
                             {http_challenge_port} typically needs privilege (grant \
                             CAP_NET_BIND_SERVICE), or set [server.tls.acme] http_challenge_port \
                             to a port a front-end forwards :80 to"
                        );
                        #[cfg(feature = "managed-pg")]
                        crate::managed_pg::emergency_stop_async().await;
                        std::process::exit(1);
                    }
                };
            let challenge_router = crate::acme::challenge::challenge_router(tokens, https_port);
            // Serve every bound listener (one for dual-stack, two for the split
            // fallback), each a child of `server_shutdown` so they tear down with
            // the main server. The router is cheap to clone (shared Arc state).
            for challenge_listener in challenge_listeners {
                let router = challenge_router.clone();
                let challenge_shutdown = server_shutdown.child_token();
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(challenge_listener, router)
                        .with_graceful_shutdown(async move {
                            challenge_shutdown.cancelled().await;
                        })
                        .await
                    {
                        tracing::error!(
                            error = %e,
                            "ACME challenge listener stopped with an error"
                        );
                    }
                });
            }

            // Build the coordinator for leader election (whatever the role) and
            // the reporter callback, then spawn the renewal loop.
            //
            // `leadership_degraded` marks the dangerous case: a distributed
            // backend was configured — multi-replica intent — but
            // `coordinator_from_config` could not build one (no DB pool, or no
            // `db` feature here) and fell back to a per-process coordinator. It is
            // keyed off both the configured backend and the actual fallback, so it
            // never fires for a genuinely single-replica `in_process` deployment.
            // When set, the renewal loop refuses to order (see `AcmeRenewalTask`)
            // rather than let every replica take a local lease and race the CA.
            let mut leadership_degraded = false;
            let coordinator =
                match crate::scheduler::coordinator_from_config(&config.scheduler, &state) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "ACME renewal: falling back to an in-process coordinator"
                        );
                        leadership_degraded = config.scheduler.backend.is_fleet_distributed();
                        std::sync::Arc::new(crate::scheduler::InProcessSchedulerCoordinator::new(
                            config.scheduler.resolved_replica_id(),
                        ))
                    }
                };
            renewal_task.leadership_degraded = leadership_degraded;

            // A distributed scheduler backend means a multi-replica deployment,
            // where ACME is not fleet-safe: see `acme_fleet_warning` for the two
            // hazards and why DNS-01 only retires one of them. Warn loudly
            // rather than silently mis-serving (#1620).
            if let Some(message) = acme_fleet_warning(config.scheduler.backend, dns01) {
                tracing::warn!(scheduler_backend = coordinator.backend(), "{message}");
            }

            #[cfg(feature = "reporting")]
            let reporter = make_acme_reporter(acme_reporters);
            #[cfg(not(feature = "reporting"))]
            let reporter = make_acme_reporter();
            // Certificate renewal is a framework-scheduled operation, so a
            // failed issuance/renewal raises #1610's `scheduled_task_failure`
            // alert — reaching the operator's configured destination (email,
            // Slack, PagerDuty) rather than only the error-reporting sink. The
            // renew-before window (default 30 days) means this fires with weeks
            // of validity left, not at expiry (#1620).
            let reporter = compose_acme_alert_reporter(reporter, &state);
            renewal_task.recovery = Some(make_acme_alert_recovery(&state));
            let renewal_shutdown = server_shutdown.child_token();
            tokio::spawn(async move {
                renewal_task
                    .run(coordinator, reporter, renewal_shutdown)
                    .await;
            });
        }

        tracing::info!(bound = %bound_desc, "Listening");

        let server_shutdown_wait = server_shutdown.clone();
        // Wrap the built router with the HTML form method-override layer at the
        // very edge — outside path and method routing — so a plain browser
        // `<form method="post">` carrying `_method=PUT|PATCH|DELETE` reaches the
        // declared handler. In axum 0.8 `Router::layer` applies middleware per
        // registered method handler, which is too late: the inner `MethodRouter`
        // returns `405` before a layered service runs. Wrapping the whole router
        // as a `tower::Service` is the documented way to run middleware before
        // route matching. `TrustedProxiesLayer` must be outermost, stamped before
        // `MethodOverrideLayer` reads `ResolvedClientIdentity` for its
        // same-origin form check.
        let after_method = tower::Layer::layer(
            &crate::middleware::MethodOverrideLayer::new()
                .with_max_scan_bytes(config.security.upload.max_request_size_bytes),
            router,
        );
        let service = tower::Layer::layer(
            &crate::security::TrustedProxiesLayer::from_config(&config.security.trusted_proxies),
            after_method,
        );
        // Spawn the serve task per transport. The arms differ only in the
        // connect-info type baked into the make-service (`SocketAddr` for TCP,
        // `UdsConnectInfo` for Unix sockets); the shutdown wiring and resulting
        // `JoinHandle<io::Result<()>>` are identical. Handlers extracting
        // `ConnectInfo<SocketAddr>` are unsupported under a Unix socket — daemon
        // mode is local and loopback-equivalent.
        // A duplicate of the listening socket is kept aside so a `SIGUSR2`
        // in-place upgrade (#1674) can hand it to a successor while this process
        // keeps serving through the original. Only a plain TCP listener can be
        // handed over in this release.
        #[cfg(unix)]
        let mut handoff_socket: Option<crate::upgrade::HandoffSocket> = None;

        let server_task = match bound_listener {
            BoundListener::Tcp(listener) => {
                #[cfg(unix)]
                {
                    match crate::upgrade::HandoffSocket::from_listener(&listener) {
                        Ok(socket) => handoff_socket = Some(socket),
                        Err(e) => tracing::warn!(
                            error = %e,
                            "could not duplicate the listening socket; in-place upgrade \
                             (SIGUSR2) will be refused for this process"
                        ),
                    }
                }
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
            // HTTPS arm: mirrors the TCP arm. The peer is a real TCP
            // `SocketAddr`, so the same `ConnectInfo<SocketAddr>`,
            // `TrustedProxiesLayer`/`ClientAddr` resolution, SSE and wss
            // streaming, and shutdown wiring apply unchanged; only the rustls
            // handshake inside the listener's `accept` differs. The no-op `tap_io`
            // wrapper lets axum's blanket `Connected<IncomingStream<TapIo<L, F>>>
            // for L::Addr` supply the peer `SocketAddr`, because the concrete
            // `SocketAddr: Connected` impl exists only for `tokio::net::TcpListener`.
            #[cfg(feature = "tls")]
            BoundListener::Tls(listener) => {
                use axum::serve::ListenerExt as _;
                let listener = listener.tap_io(|_io| {});
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
        };

        // Cancelled by the in-place upgrade watcher once a successor has taken
        // over the listening socket; the drain below then runs without the
        // load-balancer choreography a real shutdown needs (#1674).
        let upgrade_cutover = tokio_util::sync::CancellationToken::new();
        let upgrade_cutover_wait = upgrade_cutover.clone();

        // In-place upgrade watcher (#1674): on `SIGUSR2`, hand this process's
        // listening socket and designated live state to a freshly-execed build
        // and, once that build is serving, cancel `upgrade_cutover` so the
        // drain below runs.
        #[cfg(unix)]
        {
            let upgrade_config = config.server.upgrade.clone();
            let upgrade_state = state.clone();
            let cutover = upgrade_cutover.clone();
            let socket = handoff_socket.take();
            // A child of `server_shutdown`, so an ordinary drain ends the
            // watcher: it drops the duplicated listening socket (which would
            // otherwise keep the port bound and accepting into a queue nobody
            // serves for the whole drain), and drops any handoff in flight,
            // whose cleanup kills the half-started successor and unfreezes the
            // live state.
            let watcher_shutdown = server_shutdown.child_token();
            tokio::spawn(async move {
                tokio::select! {
                    () = watch_for_in_place_upgrade(
                        &upgrade_config,
                        upgrade_signal,
                        socket,
                        upgrade_state,
                        cutover,
                    ) => {}
                    () = watcher_shutdown.cancelled() => {
                        tracing::debug!(
                            "shutting down: in-place upgrade is no longer available in this \
                             process"
                        );
                    }
                }
            });
        }

        let shutdown_state = state.clone();
        let shutdown_signal_token = server_shutdown.clone();
        #[cfg(feature = "ws")]
        let websocket_shutdown = state.shutdown.clone();
        // Clone metrics so the drain-watchdog can record aborted requests.
        let shutdown_metrics = state.metrics.clone();

        // Shared timestamp: set by shutdown_task when the listener is cancelled
        // (phase 5). Main reads it after server_task completes to measure only
        // actual drain time for hook budget — not the app's full uptime.
        let drain_started_at: std::sync::Arc<std::sync::OnceLock<crate::time::MonotonicInstant>> =
            std::sync::Arc::new(std::sync::OnceLock::new());
        let drain_started_clone = std::sync::Arc::clone(&drain_started_at);
        // The shutdown task does not capture `state`, so hand it its own handle
        // on the injected clock — the drain window is measured on the same
        // monotonic timeline `hook_budget` is computed from below.
        let drain_clock = state.clock_arc();
        let drain_clock_for_task = std::sync::Arc::clone(&drain_clock);

        // Notified by main just before server_task.await (after startup hooks
        // complete). If SIGTERM arrives during startup hooks the watchdog waits
        // here so the drain deadline is always measured from when drain starts.
        let drain_phase_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let drain_phase_notify_for_watchdog = std::sync::Arc::clone(&drain_phase_notify);
        // Boolean companion so the watchdog can skip the wait when SIGTERM arrives
        // after startup has already finished (the common case).
        let server_entered_drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_entered_drain_for_watchdog = std::sync::Arc::clone(&server_entered_drain);

        // Shutdown task: the rolling-deploy lifecycle phases.
        //
        //   1. SIGTERM / Ctrl-C received
        //   2. /ready → 503  (probe flips before listener closes)
        //   3. prestop_grace elapses  (load-balancer deregistration window)
        //   4. WebSocket sessions receive close frame
        //   5. TCP listener stops accepting new connections; jobs/scheduler
        //      stop dequeuing (they share server_shutdown CancellationToken)
        //   6. In-flight requests drain within shutdown_timeout_secs; past the
        //      deadline the watchdog exits with code 1 and records
        //      autumn_shutdown_aborted_requests_total.
        //
        // Phases 7-9 (on_shutdown hooks, telemetry flush, DB pool close) run in
        // main after server_task completes, within the remaining part of the same
        // shutdown_timeout_secs budget — not an additional window.
        let shutdown_task = tokio::spawn(async move {
            // Phase 1: Wait for an OS signal — or for an in-place upgrade
            // (#1674) whose successor is already serving on this same socket.
            let cause = shutdown_signal(upgrade_cutover_wait).await;
            let upgrade_cutover = matches!(cause, DrainCause::UpgradeCutover);

            if upgrade_cutover {
                // Phases 2 and 3 exist to let a load balancer take this replica
                // out of rotation before its socket closes. An in-place upgrade
                // has no such gap to cover: the successor is already accepting
                // on the *same* listening socket, so flipping `/ready` to 503
                // would only make a live address look unhealthy, and the
                // prestop grace would delay the handover for nothing.
                tracing::info!(
                    phase = "upgrade_cutover",
                    shutdown_timeout_secs = shutdown_timeout,
                    "shutdown: successor is serving; draining without a readiness flip"
                );
            } else {
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
            }
            tracing::info!(phase = "listener_stopping", "shutdown: stopping listener");

            // Phase 4: send WebSocket close frames.
            #[cfg(feature = "ws")]
            websocket_shutdown.cancel();

            // Phase 5: stop listener and signal jobs/scheduler to stop dequeuing.
            // Record drain-start before cancelling so main gets the right hook
            // budget even in the startup-overlap case.
            let _ = drain_started_clone.set(drain_clock_for_task.monotonic());
            shutdown_signal_token.cancel();

            // Phase 6: drain watchdog. If the in-flight drain exceeds the budget,
            // record the aborted count and force a non-zero exit before hooks run.
            //
            // Always measure the deadline from when the drain actually starts, so
            // in-flight requests get the full shutdown_timeout_secs window. On a
            // normal SIGTERM after startup, server_entered_drain is already true:
            // skip the wait and sleep the full budget. On a SIGTERM during hooks,
            // wait for the notify first. Without that wait, hooks completing just
            // before the watchdog fires would let it exit(1) at once, with no
            // fresh drain window for requests that arrived after the hooks.
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
            // Web role runs no cron scheduler (workers/combined only). Skipping
            // the scheduler must not regress readiness: mark_startup_complete and
            // signal_serve_ready below still run.
            if role.runs_workers() && !tasks.is_empty() {
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
            // Release a predecessor that is waiting on this build (#1674). Only
            // now, with startup hooks done and the router serving, is it safe
            // for the old process to stop accepting — and only if this build
            // actually took over everything it was handed.
            #[cfg(unix)]
            {
                if let Err(reason) = crate::upgrade::verify_handover_complete() {
                    tracing::error!("{reason}");
                    // `process::exit` skips `on_shutdown`; stop any managed Postgres.
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(1);
                }
                // Publishing readiness is the last thing that can fail, so the
                // adopted state stays frozen until it succeeds. A readiness signal
                // that never reaches the predecessor — a full or read-only handoff
                // filesystem — means the predecessor times out and kills this
                // process, taking anything acknowledged meanwhile with it.
                // Refusing here ends that wait when this process exits, in ~20 ms
                // rather than the readiness timeout, and it resumes writable.
                match crate::upgrade::publish_upgrade_readiness() {
                    Ok(had_predecessor) => {
                        if had_predecessor {
                            unfreeze_adopted_live_state(&state);
                        }
                    }
                    Err(error) => {
                        tracing::error!(
                            error = %error,
                            "refusing to start: this build took over but could not tell the \
                             previous build it is serving, so the handover cannot complete. \
                             The previous build keeps serving"
                        );
                        // `process::exit` skips `on_shutdown`; stop any managed Postgres.
                        #[cfg(feature = "managed-pg")]
                        crate::managed_pg::emergency_stop_async().await;
                        std::process::exit(1);
                    }
                }
            }
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

        // How much of `shutdown_timeout_secs` the drain has spent so far,
        // measured on the injected clock from the instant phase 5 recorded.
        // Read twice below — once for the departure wait, once for the hook
        // budget — because everything after the drain shares that one budget.
        let elapsed_since_drain_start = || {
            drain_started_at
                .get()
                .map_or(std::time::Duration::ZERO, |started| {
                    drain_clock.monotonic().saturating_duration_since(*started)
                })
        };
        let shutdown_budget = std::time::Duration::from_secs(shutdown_timeout);

        // Phase 6a: the replication loop's final flush. Cancelling the token at
        // phase 5 only wakes that loop; the tick that ships the last committed
        // frames runs after it, and nothing waits for that tick unless this does.
        // Requests have drained, so no further transaction can commit and this is
        // the last flush there will be — the difference between a clean stop that
        // loses nothing and one that leaves the tail of the WAL only on a machine
        // that may be about to go away.
        //
        // Bounded by what the drain left of `shutdown_timeout_secs`, on the same
        // budget as the departure and the hooks below: a destination that has gone
        // away must not hold the process open past what a supervisor allows.
        // Overrunning it is loud rather than silent — the operator's RPO is at stake.
        #[cfg(feature = "db")]
        if let Some(waiter) = replication_done.take() {
            // Only now: every request that will ever commit has committed, so
            // the tick this releases is genuinely the last one.
            replication_shutdown.cancel();
            let wait = shutdown_budget.saturating_sub(elapsed_since_drain_start());
            match tokio::time::timeout(wait, waiter).await {
                Ok(Ok(())) => {
                    tracing::info!("shutdown: SQLite replication flushed and stopped");
                }
                Ok(Err(_)) => tracing::error!(
                    "shutdown: the SQLite replication thread ended without finishing; the \
                     frames committed since the last successful tick may not be offsite"
                ),
                // The thread is left running and the process exits without it.
                // Waiting further is what the budget exists to prevent, and a
                // blocking join could not be abandoned at all.
                Err(_) => tracing::warn!(
                    timeout_secs = wait.as_secs(),
                    "shutdown: SQLite replication did not finish its final flush within the \
                     shutdown budget; the last frames may not be offsite"
                ),
            }
        }

        // Phase 6b: the cluster departs only now, once no request can still be
        // running — an increment accepted during the drain must have a push loop
        // left to replicate it. Ordering, all inside the one
        // `shutdown_timeout_secs` budget: drain → departure → hooks. The departure
        // is bounded by `LEAVE_BUDGET` inside the node, and this waits for it, but
        // only for what the drain left (`cluster_departure_wait`): a supervisor
        // times the process out on `shutdown_timeout_secs`, and an unconditional
        // wait after a slow drain would push past it. The hook budget below
        // subtracts this wait from the same clock reading, so the three phases add
        // up to the budget rather than budget + `LEAVE_BUDGET`. A departure that is
        // budgeted away leaves the peer to converge on the suspicion timeout, which
        // is the actual contract.
        if config.cluster.enabled {
            cluster_shutdown.cancel();
            let departure_wait =
                cluster_departure_wait(shutdown_budget, elapsed_since_drain_start());
            if !departure_wait.is_zero() {
                tokio::time::sleep(departure_wait).await;
            }
        }

        // Phase 7: run on_shutdown hooks within the *remaining* portion of
        // shutdown_timeout_secs (drain + departure + hooks share one budget,
        // not three).
        // Plugin ordering: plugins register during build() before app hooks,
        // so app hooks run before plugin hooks (LIFO = last-registered first).
        let hook_budget = shutdown_budget.saturating_sub(elapsed_since_drain_start());
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
            plugin_contracts: _,
            plugin_config_roots,
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
            alert_channels: _,
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
            mail_suppression_store,
            #[cfg(feature = "mail")]
            mount_unsubscribe_endpoint,
            #[cfg(feature = "mail")]
            mail_previews,
            #[cfg(feature = "maud")]
            story_gallery,
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
        let (mut config, telemetry_guard) = load_config_and_telemetry(
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
        )
        .await;

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
        // Wire the live-subscriber reload handle into the loggers actuator so
        // `PUT /actuator/loggers/{name}` affects the running subscriber, not
        // just an in-memory map (issue #1044).
        if let Some(handle) = telemetry_guard.filter_reload.clone() {
            state.log_levels().attach_reload_handle(handle);
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
        if let Some(handle) = mail_suppression_store {
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
        #[cfg(feature = "maud")]
        install_story_registry(&state, story_gallery);
        // run_build_mode used ProbeState::default(), which does not start as pending
        state.probes = crate::probe::ProbeState::default();

        // Apply deferred policy and scope registrations onto the live app state,
        // as `run()` does. Static routes can carry `#[authorize]` checks or sit
        // behind `#[repository(policy = ..., scope = ...)]` index endpoints;
        // without registering here, every such pre-render call would 500 at build
        // time with "no policy/scope registered", and `render_static_routes` would
        // treat that as a build failure even though `.policy(...)`/`.scope(...)`
        // was configured on the builder.
        for register in policy_registrations {
            register(state.policy_registry());
        }

        #[cfg(feature = "i18n")]
        let custom_layers =
            install_i18n_bundle_layer(custom_layers, &state, i18n_bundle, &config.i18n);

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
        // Static routes are pre-rendered by requesting their single,
        // unprefixed path — never locale-aware — so they must stay excluded
        // from locale-prefix routing even when it's enabled (issue #1251).
        exclude_static_routes_from_locale_prefix(&mut config, &static_metas);
        // Refresh the AppState-stored config snapshot — see the matching
        // comment in `run()` (Codex review).
        state.insert_extension(config.clone());
        // Publish the architecture graph this process serves (#1747) before the
        // router is built, so `/actuator/graph` can answer from the first
        // request rather than after some later warm-up.
        crate::graph::install(crate::graph::manifest::audit(
            &graph_mounted_routes(&all_routes, &scoped_groups, &[], &config),
            // The static-build path builds its router with no nest mounts and
            // no declared plugin routes (see the `RouterContext` below), so the
            // merge count is the whole opaque surface here.
            omitted_router_count(merge_routers.len(), std::iter::empty::<&str>(), &[]),
        ));
        let router = crate::router::try_build_router_inner(
            all_routes,
            &config,
            state,
            crate::router::RouterContext {
                exception_filters: Vec::new(),
                scoped_groups,
                merge_routers,
                nest_routers: Vec::new(),
                declared_routes: Vec::new(),
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
            // A static route that declared `seo(robots = "noindex")` must not
            // be advertised in sitemap.xml — otherwise the app tells crawlers
            // "here is this URL" and "do not index it" at the same time (#1182).
            let static_paths: Vec<&str> = static_metas
                .iter()
                .filter(|m| !crate::seo::defaults_exclude_from_sitemap(m.seo))
                .map(|m| m.path)
                .collect();
            let (robots_body, sitemap_body) = crate::seo::assemble_seo_bodies(
                profile,
                seo_cfg.base_url.as_deref(),
                seo_cfg.robots.sitemap_url.as_deref(),
                &seo_cfg.robots.additional_rules,
                &seo_sources,
                &static_paths,
                sitemap_locale_config(&config),
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

    /// Dump the application's architecture graph as JSON and exit.
    ///
    /// Triggered when `AUTUMN_DUMP_GRAPH=1` is set (by `autumn graph`).
    /// Does not connect to a database or bind a TCP port.
    fn run_dump_graph_mode(&self) {
        // The framework's mounts depend on configuration — the health probe
        // paths and the actuator prefix are both configurable — so the census
        // needs the app's own config, not defaults. `AutumnConfig::load()` is
        // the plain five-layer TOML + env read with no telemetry or database
        // work, which keeps this dump's promise of touching neither. A config
        // that fails to load is not this command's error to report: fall back
        // to defaults so the graph still dumps, exactly as the routes listing
        // would still list.
        let config = crate::config::AutumnConfig::load().unwrap_or_default();
        let mounted = graph_mounted_routes(
            &self.routes,
            &self.scoped_groups,
            &self.declared_routes,
            &config,
        );
        crate::graph::manifest::print_manifest_dump(&crate::graph::manifest::audit(
            &mounted,
            self.graph_opaque_router_count(),
        ));
    }

    /// Raw `merge`/`nest` routers whose endpoints the graph cannot enumerate.
    ///
    /// The same count `autumn routes audit` hard-fails its coverage gate on
    /// (`omitted_router_count`), so the graph and the route audit cannot
    /// disagree about how much of the served surface is opaque.
    fn graph_opaque_router_count(&self) -> usize {
        omitted_router_count(
            self.merge_routers.len(),
            self.nest_routers.iter().map(|(prefix, _)| prefix.as_str()),
            &self.declared_routes,
        )
    }

    /// Dump the agent-authority manifest as one marker-prefixed JSON line and
    /// exit.
    ///
    /// Triggered when `AUTUMN_DUMP_AGENT_AUTHORITY=1` is set (by `autumn agents
    /// manifest`). Takes `&self` rather than consuming the builder: it reads
    /// the route table and the audit-sink status and touches nothing else, so
    /// there is no database to open and no port to bind.
    fn run_dump_agent_authority_mode(&self) {
        // Whether agent invocations have anywhere to be recorded is a property
        // of the deployment, not of any grant, and it belongs in the document
        // rather than in a startup line nobody reads (#1691 R9).
        let audit_sink_configured = self
            .audit_logger
            .as_ref()
            .is_some_and(|logger| logger.is_enabled());
        // The whole-API MCP hatch, when the `mcp` feature is compiled in and
        // the app opted into it. Without it every route `expose_all_as_mcp()`
        // exposes would be missing from the document entirely.
        #[cfg(feature = "mcp")]
        let expose_all = self.mcp.as_ref().is_some_and(|rt| rt.expose_all);
        #[cfg(not(feature = "mcp"))]
        let expose_all = false;
        // Top-level routes carry their own path; a scoped group's children do
        // not -- the group's prefix is applied at mount time, so the path on
        // the `Route` is the child path alone. Passing that through would
        // record `/items` for a tool an agent calls at `/api/v1/items`, and a
        // scope rename would then produce no drift at all.
        let routes: Vec<crate::agent_authority::manifest::RouteSummary> = self
            .routes
            .iter()
            .map(|route| agent_authority_route_summary(route, None, expose_all))
            .chain(self.scoped_groups.iter().flat_map(|group| {
                group.routes.iter().map(move |route| {
                    agent_authority_route_summary(route, Some(&group.prefix), expose_all)
                })
            }))
            .collect();
        crate::agent_authority::manifest::print_manifest_dump(
            &crate::agent_authority::manifest::build(&routes, audit_sink_configured),
        );
    }

    /// Dump the application's route listing as JSON and exit.
    ///
    /// Triggered when `AUTUMN_DUMP_ROUTES=1` is set (by `autumn routes`).
    /// Exits with code 0 on success, code 1 on JSON serialization failure.
    /// Does not connect to a database or bind a TCP port.
    #[allow(clippy::too_many_lines)]
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
            plugin_config_roots,
            plugin_contracts,
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

        // Raw Axum routers registered via `.merge()`/`.nest()` are opaque: no
        // public API enumerates their routes, so they are omitted from the listing
        // and hard-fail `autumn routes audit`, since their auth posture cannot be
        // proven. The exception is a `.nest(prefix, router)` whose endpoints were
        // declared through `declare_plugin_routes`: those are enumerable, folded
        // into `declared_routes`, and must not count as omitted. Every `.merge()`
        // is rootless and always counts; a bare `.nest()` with no covering
        // declaration stays opaque and counts.
        let hidden = omitted_router_count(
            merge_routers.len(),
            nest_routers.iter().map(|(prefix, _)| prefix.as_str()),
            &declared_routes,
        );
        if hidden > 0 {
            eprintln!(
                "[autumn routes] warning: {hidden} raw router(s) added via \
                 .merge()/.nest() are not enumerable and are omitted from this listing"
            );
            // Machine-readable marker consumed by `autumn routes audit` to
            // hard-fail the coverage gate: omitted routes can't be proven.
            eprintln!(
                "{marker}{hidden}",
                marker = crate::route_listing::OMITTED_ROUTES_MARKER
            );
        }

        let (config, _telemetry_guard) = load_config_and_telemetry(
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
        )
        .await;

        // Emit the resolved security configuration for the manifest's `declared`
        // dimensions (CSRF, security headers). Gated on `AUTUMN_DUMP_SECURITY`
        // so only `autumn routes audit` sees it — kept off stdout so the
        // routes-only JSON parse path stays byte-compatible.
        if is_dump_security_mode() {
            let security = crate::route_listing::SecurityDump::from_config(&config);
            match serde_json::to_string(&security) {
                Ok(json) => eprintln!(
                    "{marker}{json}",
                    marker = crate::route_listing::SECURITY_CONFIG_MARKER
                ),
                Err(e) => eprintln!("Failed to serialize security config: {e}"),
            }
        }

        // Emit the plugin compatibility contracts declared by this app's
        // plugins (issue #1601). Gated on `AUTUMN_DUMP_PLUGIN_CONTRACT` so only
        // `autumn plugin-check` sees it, and emitted even when the array is
        // empty: the CLI distinguishes "this binary declares no contracts" from
        // "this binary predates the marker" by the line's presence.
        if is_dump_plugin_contract_mode() {
            match serde_json::to_string(&plugin_contracts) {
                Ok(json) => eprintln!(
                    "{marker}{json}",
                    marker = crate::plugin_contract::PLUGIN_CONTRACT_MARKER
                ),
                Err(e) => eprintln!("Failed to serialize plugin contracts: {e}"),
            }
        }

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

    /// Dump the generated `OpenAPI` document as JSON and exit.
    ///
    /// Triggered when `AUTUMN_DUMP_OPENAPI=1` is set (by
    /// `autumn openapi export`). Does not connect to a database or bind a TCP
    /// port.
    ///
    /// The document is built through the exact same pair the `/openapi.json`
    /// route uses — [`crate::router::collect_openapi_docs`] then the spec
    /// generator — so an exported spec and a served one cannot drift. Config is
    /// loaded the same way a normal boot loads it, because the session cookie
    /// name feeds the `SessionAuth` security scheme.
    ///
    /// Like the served route this evaluates deprecation/sunset state against
    /// the current instant ([`crate::openapi::generate_spec`] passes
    /// `Utc::now()`), so an export is reproducible except across a declared
    /// deprecation or sunset date — which is a real contract change a `--check`
    /// diff should surface, not noise to suppress.
    ///
    /// Exits 0 on success, 1 on serialization failure, and 2 when the app has
    /// no spec to emit (reported on the
    /// [`OPENAPI_UNAVAILABLE_MARKER`](crate::openapi::OPENAPI_UNAVAILABLE_MARKER)
    /// protocol).
    #[cfg(feature = "openapi")]
    async fn run_dump_openapi_mode(self) {
        let Self {
            routes,
            scoped_groups,
            api_versions,
            openapi,
            config_loader_factory,
            plugin_config_roots,
            ..
        } = self;

        let Some(openapi_config) = openapi else {
            eprintln!(
                "{marker}{reason}",
                marker = crate::openapi::OPENAPI_UNAVAILABLE_MARKER,
                reason = crate::openapi::OPENAPI_UNAVAILABLE_UNCONFIGURED,
            );
            std::process::exit(2);
        };

        // Config only: `TelemetryProvider::init` can reach a collector or read
        // production credentials, and telemetry cannot affect the document, so
        // an export advertised as touching nothing must not run it.
        let config = load_config_only(config_loader_factory, plugin_config_roots).await;

        let mut openapi_config = openapi_config;
        openapi_config.api_versions = api_versions;
        let openapi_config = openapi_config.session_cookie_name(config.session.cookie_name);

        let docs = crate::router::collect_openapi_docs(&routes, &scoped_groups);
        let refs: Vec<&crate::openapi::ApiDoc> = docs.iter().collect();
        let spec = crate::openapi::generate_spec(&openapi_config, &refs);

        let json = serde_json::to_string_pretty(&spec).unwrap_or_else(|e| {
            eprintln!("Failed to serialize OpenAPI spec: {e}");
            std::process::exit(1);
        });
        println!("{json}");
        std::process::exit(0);
    }

    /// Dump the effective drained-queue manifest as TOML and exit.
    ///
    /// Triggered when `AUTUMN_DUMP_JOBS=1` is set (by `autumn jobs manifest`).
    /// Emits a single top-level `queues = [...]` array — the configured
    /// `[jobs.queues]` set unioned with every `#[job(queue = "…")]`-declared
    /// queue, ordered highest priority first exactly as the runtime drains — so a
    /// topology-aware `autumn doctor` consumes the ground-truth set the app runs
    /// with. Does not connect to a database or bind a TCP port. Always exits 0.
    async fn run_dump_jobs_mode(self) {
        let Self {
            jobs,
            listeners,
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
            ..
        } = self;

        let (config, _telemetry_guard) = load_config_and_telemetry(
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
        )
        .await;

        // Fold in the synthesized durable-listener jobs exactly as the boot path
        // does, so the manifest reflects the same effective drained-queue set the
        // runtime drains (including the `default` queue those jobs land on).
        let manifest = dump_jobs_manifest(&config.jobs.queues, jobs, listeners);
        print!("{manifest}");
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

    /// Apply pending embedded migrations and exit (the `AUTUMN_MIGRATE=1`
    /// one-shot), WITHOUT starting the HTTP server or binding a port.
    ///
    /// Reuses the exact applier the startup auto-migration path uses
    /// ([`run_pending_locked`](crate::migrate::run_pending_locked), the public
    /// wrapper over the same locked engine `auto_migrate` drives) and the same
    /// framework-migration fold ([`migrations_with_repository_framework_migrations`]),
    /// so the applied set matches a normal boot. Unlike that path it applies
    /// regardless of profile — the deploy invokes it explicitly — and it targets
    /// the writable primary(ies) only (control primary + each shard primary),
    /// exactly like `autumn migrate` / the deploy DB preflight; replicas are never
    /// migration targets. The framework-internal directory / shard-map guard
    /// tables are deliberately NOT applied here: the app applies them
    /// unconditionally at startup, so the candidate's own boot creates them.
    ///
    /// Exits 0 after applying (printing a redacted count — never a URL or secret)
    /// and 1 on the first failure, so a failed migration aborts the deploy before
    /// cutover with the old release still serving (AC-3).
    #[cfg(feature = "db")]
    #[allow(clippy::too_many_lines)]
    async fn run_migrate_only_mode(self) {
        let Self {
            migrations,
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
            ..
        } = self;

        // The telemetry guard is dropped at end of scope; a migrate-only run does
        // not need tracing wired, but loading config the same way keeps env/profile
        // resolution identical to a normal boot.
        let (config, _telemetry_guard) = load_config_and_telemetry(
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
        )
        .await;

        // Fold in the framework migration sets a normal boot would apply, using the
        // SAME helper as `setup_database`, so the applied set is identical.
        let migrations = migrations_with_repository_framework_migrations(
            migrations,
            crate::repository_commit_hooks::has_repository_commit_hook_descriptors(),
            crate::version_history::has_versioned_repository_descriptors(),
            RepositoryCommitHookQueueMigrationMode::Runtime,
        );

        // Writable targets only: the control primary, then each shard primary.
        let control_url = config.database.effective_primary_url().map(str::to_owned);
        let shard_targets: Vec<(String, String)> = config
            .database
            .shards
            .iter()
            .map(|shard| (format!("shard:{}", shard.name), shard.primary_url.clone()))
            .collect();

        if migrations.is_empty() || (control_url.is_none() && shard_targets.is_empty()) {
            eprintln!(
                "autumn migrate: no database configured or no migrations registered — nothing to apply"
            );
            std::process::exit(0);
        }

        // SQLite migrate-only guard (#1614, PR3). Sharding is Postgres-only, so a
        // `sqlite:` control target with shards configured, or any `sqlite:` shard
        // target, fails fast here with the actionable sharding error. It is the
        // same `sqlite_sharding_unsupported_guard` normal boot applies, so the two
        // paths cannot drift. A plain `sqlite:` control target with no shards is
        // not gated: the SQLite apply path in the loop below handles its
        // migrations. An all-Postgres or empty-shard configuration is never gated.
        #[cfg(feature = "sqlite")]
        {
            let sqlite_guard_shard_urls: Vec<&str> =
                shard_targets.iter().map(|(_, url)| url.as_str()).collect();
            if let Err(e) = sqlite_sharding_unsupported_guard(
                control_url.as_deref(),
                !shard_targets.is_empty(),
                &sqlite_guard_shard_urls,
            ) {
                eprintln!("autumn migrate: {e}");
                // `process::exit` skips `on_shutdown`/`Drop`; stop any managed
                // Postgres child first, mirroring `apply_pending_or_exit`.
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop();
                std::process::exit(1);
            }
        }

        // Computed once, on the FINAL registered set (after the fold above),
        // so a version collision resolves automatically instead of one
        // migration silently never applying — see
        // `compute_migration_disambiguation`. `migration_sets_for_disambiguation`
        // folds in the two standalone shard control sets too, matching
        // `run_startup_migrations`, so both paths reach the same decision
        // regardless of which runs first.
        let disambiguation_sets =
            migration_sets_for_disambiguation(&migrations, config.database.has_shards());
        let disambiguated = crate::migrate::compute_migration_disambiguation(&disambiguation_sets);

        // The diesel harness and the advisory-lock poll block, so apply off the
        // Tokio worker threads. Each target's failure exits non-zero from inside.
        let applied_total = tokio::task::spawn_blocking(move || {
            let mut total = 0_usize;
            if let Some(url) = &control_url {
                // SQLite single-writer control target (issue #1614, PR3): apply with
                // NO advisory lock via the SQLite harness. Sharding is rejected above,
                // so a SQLite control target here is always unsharded (shard_targets
                // is empty). Every non-SQLite target keeps the byte-identical locked
                // Postgres applier.
                #[cfg(feature = "sqlite")]
                let is_sqlite_control = crate::config::DatabaseBackend::detect(url)
                    == Some(crate::config::DatabaseBackend::Sqlite);
                #[cfg(not(feature = "sqlite"))]
                let is_sqlite_control = false;
                if is_sqlite_control {
                    #[cfg(feature = "sqlite")]
                    for (_, mig) in &migrations {
                        total += apply_pending_sqlite_or_exit(
                            url,
                            crate::migrate::DisambiguatedMigrations::new(mig, &disambiguated),
                            "control",
                        );
                    }
                } else {
                    for (_, mig) in &migrations {
                        total += apply_pending_or_exit(
                            url,
                            crate::migrate::DisambiguatedMigrations::new(mig, &disambiguated),
                            "control",
                        );
                    }
                }
            }
            // Shards hold tenant data, not the control-plane schema; skip the
            // control framework set for shard targets (mirrors `run_startup_migrations`).
            // A `sqlite:` shard is rejected by the guard above, so every shard here
            // is Postgres.
            for (label, url) in &shard_targets {
                for (_, mig) in migrations
                    .iter()
                    .filter(|(_, mig)| !migration_set_is_control_framework(mig))
                {
                    total += apply_pending_or_exit(
                        url,
                        crate::migrate::DisambiguatedMigrations::new(mig, &disambiguated),
                        label,
                    );
                }
            }
            total
        })
        .await
        .unwrap_or_else(|error| {
            eprintln!("autumn migrate: migration task panicked: {error}");
            std::process::exit(1);
        });

        eprintln!(
            "autumn migrate: applied {applied_total} pending migration(s); database is up to date"
        );
        std::process::exit(0);
    }

    /// The `AUTUMN_MIGRATE=1` one-shot on a build compiled WITHOUT database
    /// support: there is nothing to migrate, so report and exit 0 (never starting
    /// the server) so a DB-free app's deploy still runs the step harmlessly.
    #[cfg(not(feature = "db"))]
    #[allow(clippy::unused_async)]
    async fn run_migrate_only_mode(self) {
        eprintln!("autumn migrate: this build has no database support — nothing to migrate");
        std::process::exit(0);
    }

    /// Count (never delete) the rows every registered
    /// `#[repository(..., retention(...))]` policy would sweep right now,
    /// print the report as JSON, and exit.
    ///
    /// Triggered by `AUTUMN_RETENTION_DRY_RUN=1` from `autumn retention
    /// --dry-run` (issue #1342). Boots just enough context to query the
    /// database — no HTTP listener, no job/mail/i18n machinery — mirroring
    /// `run_migrate_only_mode`'s minimal footprint.
    #[cfg(feature = "db")]
    async fn run_retention_dry_run_mode(self) {
        let Self {
            tasks,
            migrations,
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
            pool_provider_factory,
            shard_provider_factory,
            shard_router,
            directory_shard_router,
            #[cfg(feature = "ws")]
            channels_backend,
            ..
        } = self;

        let model_filter = retention_dry_run_model_filter_from_env();
        // Resolve/validate the requested policy selection BEFORE connecting
        // to the database (#1342 review round 9): an app with no
        // retention(...) policies at all, or a --model that names nothing
        // registered, can answer without ever opening a connection — every
        // check here reads only the compile-time-registered descriptor set.
        // A real database only gets touched once we know a policy actually
        // needs to be counted.
        let descriptors =
            match crate::retention::resolve_retention_descriptors(model_filter.as_deref()) {
                Ok(descriptors) => descriptors,
                Err(error) => {
                    eprintln!("retention dry-run: {error}");
                    std::process::exit(1);
                }
            };

        // `resolve_retention_descriptors` validates collisions only among
        // retention-generated task names. It cannot see hand-declared `tasks![...]`
        // entries, which real boot merges in and validates through
        // `validate_unique_scheduled_task_names`. Without this check a dry run could
        // report success for a policy whose generated name collides with a
        // hand-declared task, while real boot panics on that collision. `tasks` is
        // carried into this mode, rather than discarded via `..`, so this check can
        // run.
        //
        // Merged against every registered retention descriptor, not just the
        // `descriptors` a `--model` filter narrowed to: real boot has no filter
        // concept, so a hand-declared task colliding with an unselected policy's
        // generated name would still panic real boot.
        if let Err(error) =
            merge_and_validate_task_names(&crate::retention::all_retention_descriptors(), tasks)
        {
            eprintln!("retention dry-run: {error}");
            std::process::exit(1);
        }

        if descriptors.is_empty() {
            println!("{RETENTION_DRY_RUN_JSON_PREFIX}[]");
            std::process::exit(0);
        }

        let (config, _telemetry_guard) = load_config_and_telemetry(
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
        )
        .await;

        // A `match`, not `.unwrap_or_else` (#1342 review round 15): a
        // managed-Postgres provider may have already started its postmaster
        // by the time `setup_database` fails (e.g. `pool_size = 0` failing
        // the deadpool build in `create_pool`, after the postmaster is up).
        // `emergency_stop_async()` is async, and `.unwrap_or_else`'s closure
        // can't `.await` — matching the same restructuring every later exit
        // in this function already uses (#1342 review round 6).
        let database = match setup_database(
            &config,
            migrations,
            pool_provider_factory,
            shard_provider_factory,
            shard_router,
            directory_shard_router,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        )
        .await
        {
            Ok(database) => database,
            Err(error) => {
                eprintln!("{error}");
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop_async().await;
                std::process::exit(1);
            }
        };

        let state = build_state(
            &config,
            database.topology.as_ref(),
            database.shards,
            #[cfg(feature = "ws")]
            channels_backend,
        );

        // `process::exit` below skips `on_shutdown` — including a managed-Postgres
        // `stop()` — so every exit from this one-shot would otherwise leave the
        // postmaster `setup_database` may have started running, with its data
        // directory locked for later commands (#1342 review round 6). Stop it
        // explicitly before every exit rather than relying on `on_shutdown`.
        match crate::retention::run_retention_dry_run(&state, model_filter.as_deref()).await {
            Ok(reports) => match serde_json::to_string(&reports) {
                Ok(json) => {
                    println!("{RETENTION_DRY_RUN_JSON_PREFIX}{json}");
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(0);
                }
                Err(error) => {
                    eprintln!("retention dry-run: failed to serialize report: {error}");
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(1);
                }
            },
            Err(error) => {
                eprintln!("retention dry-run: {error}");
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop_async().await;
                std::process::exit(1);
            }
        }
    }

    /// Report or enforce the unified `[retention]` policy over every
    /// framework-owned dataset, print the report as JSON, and exit
    /// (issue #1605).
    ///
    /// Triggered by `AUTUMN_DB_RETENTION=report|purge` from `autumn db
    /// retention`. Boots just enough context to answer honestly — the
    /// resolved config, the database, and the app's own state initializers,
    /// which is what installs the [`crate::gdpr::GdprRegistry`] a legal hold
    /// lives in and the [`crate::audit::AuditLogger`] the sweep records
    /// through — but no HTTP listener and no job/scheduler machinery.
    ///
    /// Running inside the app rather than from the standalone CLI is what
    /// makes the report trustworthy: it calls the *same*
    /// [`crate::data_retention::run_retention`] the scheduled sweep calls, so
    /// what the CLI prints and what the app enforces cannot drift.
    #[allow(clippy::too_many_lines)]
    async fn run_framework_retention_mode(self, mode: FrameworkRetentionMode) {
        let Self {
            state_initializers,
            audit_logger,
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
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
            #[cfg(feature = "ws")]
            channels_backend,
            ..
        } = self;

        let dataset_filter = framework_retention_dataset_from_env();
        // Reject a mistyped `--dataset` before opening a connection: the
        // registry is compile-time, so this needs no database at all.
        if let Some(filter) = dataset_filter.as_deref()
            && crate::data_retention::RetentionDataset::from_key(filter).is_none()
        {
            let known: Vec<&str> = crate::data_retention::RETENTION_DATASETS
                .iter()
                .map(|dataset| dataset.key())
                .collect();
            eprintln!(
                "autumn db retention: unknown dataset {filter:?}; known datasets: {}",
                known.join(", ")
            );
            std::process::exit(1);
        }

        let (config, _telemetry_guard) = load_config_and_telemetry(
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
        )
        .await;

        #[cfg(feature = "db")]
        let database = match setup_database(
            &config,
            migrations,
            pool_provider_factory,
            shard_provider_factory,
            shard_router,
            directory_shard_router,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        )
        .await
        {
            Ok(database) => database,
            Err(error) => {
                eprintln!("{error}");
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop_async().await;
                std::process::exit(1);
            }
        };

        let state = build_state(
            &config,
            #[cfg(feature = "db")]
            database.topology.as_ref(),
            #[cfg(feature = "db")]
            database.shards,
            #[cfg(feature = "ws")]
            channels_backend,
        );
        // `AppBuilder::with_audit_sink(...)` installs its logger here, not via
        // a state initializer. Skipping it would leave an on-demand purge with
        // no audit record at all, and would make
        // `--dataset audit_archives` a silent no-op for exactly the apps that
        // use the first-class builder.
        if let Some(logger) = audit_logger {
            state.insert_extension::<crate::audit::AuditLogger>((*logger).clone());
        }
        // The app's own state initializers are what install the GDPR registry
        // a legal hold lives in. Skipping them would make `autumn db
        // retention` report a sweep the running app would actually refuse.
        run_state_initializers(state_initializers, &state);

        let options = crate::data_retention::RetentionRunOptions {
            dry_run: mode == FrameworkRetentionMode::Report,
            dataset: dataset_filter.as_deref(),
        };
        // `process::exit` skips `on_shutdown`, so a managed-Postgres
        // postmaster `setup_database` may have started must be stopped
        // explicitly before every exit below.
        match crate::data_retention::run_retention(&state, &options).await {
            Ok(reports) => match serde_json::to_string(&reports) {
                Ok(json) => {
                    println!("{FRAMEWORK_RETENTION_JSON_PREFIX}{json}");
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    // A dataset that failed is reported in its own row rather
                    // than aborting the run, but the command as a whole must
                    // still exit non-zero so a scripted purge doesn't look
                    // successful.
                    let exit_code = i32::from(reports.iter().any(|r| r.error.is_some()));
                    std::process::exit(exit_code);
                }
                Err(error) => {
                    eprintln!("autumn db retention: failed to serialize report: {error}");
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(1);
                }
            },
            Err(error) => {
                eprintln!("autumn db retention: {error}");
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop_async().await;
                std::process::exit(1);
            }
        }
    }

    /// The `AUTUMN_RETENTION_DRY_RUN=1` one-shot on a build compiled WITHOUT
    /// database support: there is nothing to sweep, so report and exit 0
    /// (never starting the server).
    ///
    /// Still prints `[]` to stdout (framed by
    /// [`RETENTION_DRY_RUN_JSON_PREFIX`]) — `autumn retention --dry-run`
    /// always looks for that framed report line, so silently printing
    /// nothing here would surface as a parse failure instead of the
    /// intended "no policies" result.
    #[cfg(not(feature = "db"))]
    #[allow(clippy::unused_async)]
    async fn run_retention_dry_run_mode(self) {
        eprintln!(
            "autumn retention --dry-run: this build has no database support — nothing to report"
        );
        println!("{RETENTION_DRY_RUN_JSON_PREFIX}[]");
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
            mail_suppression_store,
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
            plugin_config_roots,
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

        let (config, telemetry_guard) = load_config_and_telemetry(
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
        )
        .await;

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
        // Wire the live-subscriber reload handle into the loggers actuator so
        // `PUT /actuator/loggers/{name}` affects the running subscriber, not
        // just an in-memory map (issue #1044).
        if let Some(handle) = telemetry_guard.filter_reload.clone() {
            state.log_levels().attach_reload_handle(handle);
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
        if let Some(handle) = mail_suppression_store {
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
        let _custom_layers =
            install_i18n_bundle_layer(custom_layers, &state, i18n_bundle, &config.i18n);

        #[cfg(feature = "storage")]
        let _storage_router = storage_bootstrap.and_then(|bootstrap| bootstrap.install(&state));
        run_state_initializers(state_initializers, &state);
        finalize_event_bus(listeners, &mut jobs, &state);

        let task_shutdown = tokio_util::sync::CancellationToken::new();
        if let Err(error) = initialize_job_runtime(jobs, &state, &task_shutdown, &config.jobs, true)
        {
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

        // Postgres-only durable commit-hook worker; not spawned under sqlite
        // (the runtime pool is a SQLite pool the Postgres worker cannot drive).
        #[cfg(all(feature = "db", not(feature = "sqlite")))]
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
        #[cfg(all(feature = "db", not(feature = "sqlite")))]
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
        // SQLite durable commit-hook worker on the one-off task-runner path
        // (#1996 item 5); see the server-path spawn above for the rationale.
        #[cfg(all(feature = "db", feature = "sqlite"))]
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

    /// Replay a recorded failure capsule against this application and exit with
    /// the verdict (issue #1598).
    ///
    /// Triggered by `AUTUMN_REPLAY_CAPSULE=<path>`, which `autumn replay` sets.
    /// The capsule supplies the request, every clock reading and every database
    /// answer, so this path must not reach anything outside it. Exits `0` when
    /// the recorded failure reproduced, `1` when it did not (a different
    /// outcome, code that left the recorded database tape, or code that
    /// reached the recorded outcome without asking for all of it), `2` when the
    /// capsule was refused and nothing ran at all — which includes any capsule
    /// marked truncated, such as one whose request used an unrecorded
    /// `[[database.shards]]` connection.
    ///
    /// # Deltas from [`run`](Self::run)
    ///
    /// **Kept**, because a replay against a stripped-down app is not a replay:
    /// the app's own configuration, the route table and scoped groups, merged
    /// and nested routers, custom Tower layers, the exception-filter chain, the
    /// error-page renderer, the i18n bundle, the custom session store, policy
    /// and scope registrations, state initializers, the DB interceptor, and the
    /// real router builder ([`try_build_router_inner`](crate::router::try_build_router_inner)).
    /// Kept app code is *not* fail-closed: a state initializer that dials an
    /// external service itself (a feature-flag SDK with its own HTTP stack)
    /// still dials it here — the offline guarantees below cover the
    /// framework's seams, and the guide's "Limitations" section says so.
    ///
    /// **Dropped** (F15 — a replay is offline by construction):
    ///
    /// * `setup_database` is never called: no pool is dialled and no migration
    ///   runs. The pool is
    ///   [`pool_from_capsule`](crate::capsule::pool_from_capsule), which answers
    ///   from the capsule's recorded wire traffic over an in-process pipe.
    /// * Capture is never armed — `[failure_capture]` is forced off, so no
    ///   [`CaptureLayer`](crate::capsule::CaptureLayer) is installed, no
    ///   request carries a capture scope, and the replay neither points the
    ///   checkout marker at the stub pool nor writes a capsule of itself.
    /// * The session store is forced to memory and the process cache is
    ///   cleared, so no Redis or external cache is dialled. A store installed
    ///   with [`with_session_store`](Self::with_session_store) is **dropped**
    ///   rather than forwarded: it outranks the config in `apply_session_layer`,
    ///   so passing it through would let a replay reach — and write to — the
    ///   application's real session backend.
    /// * Channels are forced in-process and a backend installed with
    ///   [`with_channels_backend`](Self::with_channels_backend) is **dropped**,
    ///   for the same reason as the session store: the Redis backend spawns a
    ///   publisher and a listener against the application's live fan-out as
    ///   soon as the state is built.
    /// * A config loader installed with
    ///   [`with_config_loader`](Self::with_config_loader) is **dropped**:
    ///   the documented implementations call AWS Secrets Manager, Vault,
    ///   Consul, or an HTTP endpoint, and an offline replay must neither
    ///   contact production infrastructure nor abort because it is
    ///   unreachable. Configuration loads from local files and the
    ///   environment; the secrets a loader fetches feed subsystems replay
    ///   forces off or serves from the capsule.
    /// * The global request timeout is cleared. A replay's inputs all come from
    ///   the capsule, but the timeout layer runs on real tokio timers, so a
    ///   breakpoint held in a debugger would otherwise cancel the handler and
    ///   report a mismatch that never happened. (A per-route
    ///   `#[timeout(...)]` override lives in the route table and still applies.)
    /// * Outbound HTTP is blocked wholesale
    ///   (`http_client::block_outbound_for_replay`). The state carries the
    ///   application's real `reqwest` client, and a capsule records no HTTP
    ///   responses — so a handler that calls a third party gets a clear error
    ///   naming the block, which surfaces as a mismatch rather than as a
    ///   request to a live service.
    /// * No job runtime, no scheduler, no startup/shutdown hooks, and only
    ///   *sync* event listeners (a durable listener needs the job runtime).
    /// * No storage preflight, no mailer, no fail-fast configuration gates: a
    ///   machine replaying a production capsule generally has none of that
    ///   configured, and none of it is on the recorded path. A handler that
    ///   extracts one of those subsystems is reported as a mismatch rather than
    ///   killing the replay.
    /// * No port is bound.
    #[cfg(feature = "reporting")]
    #[allow(clippy::too_many_lines)]
    async fn run_replay_mode(self, capsule_path: String) {
        let Self {
            routes,
            api_versions,
            listeners,
            exception_filters,
            scoped_groups,
            merge_routers,
            nest_routers,
            // Bound rather than dropped with the rest: a nested router is
            // opaque, so these declarations are the ONLY thing that lets the
            // collision preflight see inside a sandboxed plugin's mount.
            // Replaying with the nests but without them would skip the checks
            // and reach the axum mount panic the preflight exists to replace
            // with a refusal.
            declared_routes,
            custom_layers,
            state_initializers,
            config_loader_factory,
            telemetry_provider,
            // Kept (rather than dropped with the rest of the runtime) so a
            // *job-entry* capsule can dispatch the recorded job's handler
            // (#1634). No job runtime, scheduler or backend is started: the
            // handler is called directly, exactly once, with the recorded
            // payload — through the application's `JobInterceptor` when it
            // registered one, since that is part of how the recorded run
            // executed and dropping it would replay a different path.
            jobs,
            job_interceptor,
            // F15: deliberately *not* destructured. A store installed with
            // `with_session_store(...)` outranks `config.session.backend` in
            // `apply_session_layer`, so forwarding it would let a replay dial —
            // and mutate — the application's live Redis or database session
            // backend, or fail 503 when that backend is unreachable. Replay
            // builds its router with no custom store, so the memory backend
            // `force_offline_replay_config` sets is what actually applies.
            session_store: _replay_ignores_custom_session_store,
            policy_registrations,
            #[cfg(feature = "db")]
            db_interceptor,
            // F15, same reasoning as the session store: a backend installed
            // with `with_channels_backend(...)` outranks `config.channels`, so
            // forwarding it would let a replay publish into — and subscribe to
            // — the application's live Redis fan-out. Replay builds its state
            // with no custom backend, so the in-process backend
            // `force_offline_replay_config` selects is what actually applies.
            #[cfg(feature = "ws")]
                channels_backend: _replay_ignores_custom_channels_backend,
            #[cfg(feature = "i18n")]
            i18n_bundle,
            #[cfg(feature = "i18n")]
            i18n_auto_load,
            #[cfg(feature = "maud")]
            error_page_renderer,
            #[cfg(feature = "embed-assets")]
            embedded_static,
            #[cfg(all(feature = "embed-assets", feature = "i18n"))]
            embedded_locales,
            plugin_config_roots,
            ..
        } = self;

        // Nothing outside the capsule may be reached from here on: the router
        // this rebuilds is the real one, with the application's real outbound
        // HTTP client in its state (AC4).
        #[cfg(feature = "http-client")]
        crate::http_client::block_outbound_for_replay();

        let path = std::path::PathBuf::from(&capsule_path);
        let capsule = match crate::capsule::load_capsule(&path) {
            Ok(capsule) => capsule,
            Err(error) => {
                std::process::exit(crate::capsule::print_refusal(&error.to_string(), &path))
            }
        };
        if let Some(reason) = crate::capsule::refusal_reason(&capsule) {
            std::process::exit(crate::capsule::print_refusal(&reason, &path));
        }

        // F15: the configured telemetry provider — the default OTLP batch
        // exporter, or a custom Datadog/Sentry initializer — is replaced with
        // a logging-only one. An offline replay must not flush spans to a live
        // collector, and must not abort before its verdict because that
        // collector is unreachable from the machine doing the replaying.
        let _replay_ignores_custom_telemetry_provider = telemetry_provider;
        // A custom config loader is a live service call — the documented
        // implementations reach AWS Secrets Manager, Vault, Consul, or an HTTP
        // endpoint — and an offline replay must neither contact production
        // infrastructure nor abort because it is unreachable. Configuration comes
        // from the local files and environment instead. The values replay consumes
        // — routes, middleware, the filter list, the profile — live there, and the
        // secrets a loader fetches feed subsystems replay forces off or serves from
        // the capsule.
        let _replay_ignores_custom_config_loader = config_loader_factory;
        let (mut config, telemetry_guard) = load_config_and_telemetry(
            None,
            Some(Box::new(crate::telemetry::ReplayTelemetryProvider)),
            plugin_config_roots,
        )
        .await;
        force_offline_replay_config(&mut config);

        // A capsule from a *different application* can replay "successfully"
        // against this one when both expose the same route shape — a verdict
        // that looks authoritative and is meaningless. Same-name is not
        // provable (service names drift), so this warns loudly instead of
        // refusing; the human summary makes the mismatch impossible to miss.
        if let Some(recorded_app) = capsule.app.name.as_deref() {
            let this_app = config.telemetry.service_name.as_str();
            if recorded_app != this_app {
                tracing::warn!(
                    recorded_app,
                    this_app,
                    "the capsule was recorded by a different application; a verdict against \
                     this one may be meaningless"
                );
                eprintln!(
                    "warning: capsule was recorded by application {recorded_app:?}, but this \
                     build is {this_app:?} — if this is not a renamed service, the verdict \
                     below compares apples to oranges"
                );
            }
        }

        #[cfg(feature = "embed-assets")]
        register_embedded_static_dir(embedded_static);
        #[cfg(all(feature = "embed-assets", feature = "i18n"))]
        let i18n_bundle = embedded_i18n_bundle(i18n_bundle, embedded_locales, &config);
        #[cfg(feature = "i18n")]
        let i18n_bundle =
            resolve_i18n_bundle(i18n_bundle, i18n_auto_load, &config, &crate::config::OsEnv);

        // One log, shared: the stub connections write into it while the router
        // runs, and the verdict reads it once the router is done.
        let divergences = std::sync::Arc::new(crate::capsule::DivergenceLog::new());
        #[cfg(feature = "db")]
        let topology = replay_database_topology(&capsule, &divergences, &path);

        let mut state = build_state(
            &config,
            #[cfg(feature = "db")]
            topology.as_ref(),
            #[cfg(feature = "db")]
            None,
            #[cfg(feature = "ws")]
            None,
        );

        // Time, randomness and the effect seams are all inputs like any other:
        // serve what the capture took, in order (#1598 for the clock, #1634
        // for entropy and the effect tape).
        let fixtures = crate::capsule::ReplayFixtures::from_capsule(&capsule);
        state = state.with_clock(fixtures.clock());
        state = state.with_entropy(fixtures.entropy());
        if let Some(buf) = telemetry_guard.log_buffer.clone() {
            state.insert_extension(buf);
        }
        // No startup barrier is applied on this path (`try_build_router_inner`
        // does not add one), and a replayed request should meet the app as a
        // warm process, not one still starting.
        state.probes = crate::probe::ProbeState::default();
        state.insert_extension(RegisteredApiVersions(api_versions));
        #[cfg(feature = "db")]
        if let Some(interceptor) = db_interceptor {
            state.insert_extension(interceptor);
        }
        crate::cache::clear_global_cache();

        for register in policy_registrations {
            register(state.policy_registry());
        }

        #[cfg(feature = "i18n")]
        let custom_layers =
            install_i18n_bundle_layer(custom_layers, &state, i18n_bundle, &config.i18n);

        install_webhook_registry(&state, &config);
        run_state_initializers(state_initializers, &state);
        // Durable listeners need the job runtime this path never starts, so —
        // as in static builds — only sync listeners are registered, and a
        // durable side effect is a clean no-op.
        let sync_listeners: Vec<_> = listeners
            .into_iter()
            .filter(|listener| listener.mode == crate::events::DispatchMode::Sync)
            .collect();
        finalize_event_bus(sync_listeners, &mut Vec::new(), &state);
        // Refresh the AppState-stored config snapshot — see the matching
        // comment in `run()`.
        state.insert_extension(config.clone());

        // Cloned before the builder consumes it, so a job capsule can dispatch
        // its handler against the same rebuilt state the router serves from.
        let router_state = state.clone();
        // See the matching call in `run()`: the graph is published before the
        // router is built so `/actuator/graph` answers from the first request.
        crate::graph::install(crate::graph::manifest::audit(
            &graph_mounted_routes(&routes, &scoped_groups, &declared_routes, &config),
            omitted_router_count(
                merge_routers.len(),
                nest_routers.iter().map(|(prefix, _)| prefix.as_str()),
                &declared_routes,
            ),
        ));
        let router = crate::router::try_build_router_inner(
            routes,
            &config,
            state,
            crate::router::RouterContext {
                exception_filters,
                scoped_groups,
                merge_routers,
                nest_routers,
                declared_routes,
                custom_layers,
                static_gate_layers: Vec::new(),
                #[cfg(feature = "maud")]
                error_page_renderer,
                session_store: None,
                #[cfg(feature = "openapi")]
                openapi: None,
                #[cfg(feature = "mcp")]
                mcp: None,
            },
        )
        .unwrap_or_else(|error| {
            std::process::exit(crate::capsule::print_refusal(
                &format!("the application's router could not be rebuilt: {error}"),
                &path,
            ))
        });

        // A job capsule has no request to drive: dispatch the recorded job's
        // handler instead, with the same clock, entropy and effect tape.
        let outcome = if let Some(job) = capsule.job.as_ref() {
            let Some(info) = jobs.iter().find(|info| info.name == job.name) else {
                std::process::exit(crate::capsule::print_refusal(
                    &format!(
                        "the capsule records a failure in job {:?}, which this build does not \
                         register; replay it against the build that ran it, or add the job to \
                         `AppBuilder::jobs()`",
                        job.name
                    ),
                    &path,
                ));
            };
            let handler = info.handler;
            let job_state = router_state.clone();
            if let Some(interceptor) = job_interceptor {
                job_state.insert_extension(interceptor);
            }
            let job_name = job.name.clone();
            let dispatch: crate::capsule::JobDispatch = Box::new(move |payload| {
                Box::pin(async move {
                    crate::job::run_handler_with_interceptor(&job_name, handler, job_state, payload)
                        .await
                })
            });
            crate::capsule::execute_job(dispatch, &capsule, divergences, &fixtures).await
        } else {
            crate::capsule::execute(router, &capsule, divergences, &fixtures).await
        };
        std::process::exit(crate::capsule::print_verdict(&outcome, &path));
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

/// Whether the process should dump the generated `OpenAPI` document and exit.
///
/// Set by `autumn openapi export`. Unlike the routes dump this is checked even
/// when the `openapi` feature is off, so the CLI gets an explicit "no spec here"
/// answer instead of a booted server.
pub(crate) fn is_dump_openapi_mode() -> bool {
    std::env::var("AUTUMN_DUMP_OPENAPI").as_deref() == Ok("1")
}

/// Whether the dump should also emit the declared plugin contracts
/// ([`PLUGIN_CONTRACT_MARKER`](crate::plugin_contract::PLUGIN_CONTRACT_MARKER)).
///
/// Set by `autumn plugin-check`, which needs the contracts to report
/// experimental-surface use. The plain `autumn routes` listing does not set it,
/// so its stderr is unchanged.
pub(crate) fn is_dump_plugin_contract_mode() -> bool {
    std::env::var("AUTUMN_DUMP_PLUGIN_CONTRACT").as_deref() == Ok("1")
}

/// Whether the dump should also emit the resolved security configuration
/// ([`SECURITY_CONFIG_MARKER`](crate::route_listing::SECURITY_CONFIG_MARKER)).
///
/// Set by `autumn routes audit` (which needs the CSRF / headers config to build
/// the `declared` manifest dimensions) but not by the plain `autumn routes`
/// listing, so that command's stderr stays free of the marker line.
pub(crate) fn is_dump_security_mode() -> bool {
    std::env::var("AUTUMN_DUMP_SECURITY").as_deref() == Ok("1")
}

pub(crate) fn is_dump_jobs_mode() -> bool {
    std::env::var("AUTUMN_DUMP_JOBS").as_deref() == Ok("1")
}

/// The mounted-route table the architecture graph joins against (issue #1747).
///
/// Top-level routes carry their own path; a scoped group's children do not --
/// the group's prefix is applied at mount time -- so each child is summarised
/// with its group prefix.
fn graph_mounted_routes(
    routes: &[Route],
    scoped_groups: &[ScopedGroup],
    declared_routes: &[crate::route_listing::RouteInfo],
    config: &crate::config::AutumnConfig,
) -> Vec<crate::graph::MountedRoute> {
    // The framework's own mounts — probes, the actuator, htmx assets, the docs
    // UI — are served by `router.rs` and belong in the census: the manifest and
    // the guide both promise a framework endpoint is *named* in
    // `unmodelled_mounted_routes`, and without them the completeness report
    // systematically understated the served surface (Codex round 5). Built with
    // the same helper `autumn routes` uses, so the two cannot disagree about
    // what the framework mounts.
    let mut framework: Vec<crate::route_listing::RouteInfo> = Vec::new();
    crate::route_listing::append_framework_routes(&mut framework, config);

    routes
        .iter()
        .map(|route| graph_route_summary(route, None))
        .chain(scoped_groups.iter().flat_map(|group| {
            group
                .routes
                .iter()
                .map(move |route| graph_route_summary(route, Some(&group.prefix)))
        }))
        .chain(declared_routes.iter().map(graph_declared_route_summary))
        .chain(framework.iter().map(graph_declared_route_summary))
        .collect()
}

/// The architecture-graph view of a route declared through
/// `declare_plugin_routes` (issue #1747).
///
/// These are real served endpoints behind an otherwise opaque `nest` mount, and
/// declaring them is what stops `omitted_router_count` counting that nest as
/// unenumerable. Without them here the graph would report *neither* an opaque
/// router nor a mounted route for that surface — a hole that reads as complete
/// coverage. They carry no `#[route]` descriptor in this binary, so they land
/// in `unmodelled_mounted_routes`: named, which is the honest answer, rather
/// than silently absent.
fn graph_declared_route_summary(
    route: &crate::route_listing::RouteInfo,
) -> crate::graph::MountedRoute {
    let mut roles = route.roles.clone();
    roles.sort();
    roles.dedup();
    let mut scopes = route.scopes.clone();
    scopes.sort();
    scopes.dedup();
    crate::graph::MountedRoute {
        method: route.method.clone(),
        path: route.path.clone(),
        handler: route.handler.clone(),
        module_path: route.module.clone().unwrap_or_default(),
        auth: crate::graph::RouteAuth {
            secured: route.classification == crate::route_listing::RouteClassification::Gated,
            roles,
            scopes,
            policy: route.policy,
            // A `RouteInfo`'s `classification` already comes from
            // `route_listing::classify`, which folds a repository's own policy
            // and scope guard into `Gated` — so unlike the `Route` path above
            // there is nothing further to recover here, and no separate
            // repository scope to name.
            repository_scope: false,
            public: route.classification == crate::route_listing::RouteClassification::Public,
        },
        // A `RouteInfo` carries no repository metadata: these are plugin and
        // framework mounts, which no `#[repository]` generated.
        repository_api: None,
    }
}

/// The architecture-graph view of one mounted route (issue #1747).
///
/// The *mounted* path, not the declared one: a scoped group's children carry
/// only their child path on the `Route`, and recording `/items` for a route an
/// operator calls at `/api/v1/items` would make a scope rename invisible.
/// `join_nested_path` is the same helper the `OpenAPI` collector and the
/// agent-authority manifest use, so the three cannot disagree about where a
/// route lives.
///
/// The auth posture is read straight off the route's `ApiDoc` rather than
/// derived a second time here: `#[secured]`/`#[authorize]`/`#[public]` already
/// populate it, and a second derivation is a second thing to drift.
fn graph_route_summary(route: &Route, scope_prefix: Option<&str>) -> crate::graph::MountedRoute {
    let mut roles: Vec<String> = route
        .api_doc
        .required_roles
        .iter()
        .map(|r| (*r).to_owned())
        .collect();
    roles.sort();
    roles.dedup();
    let mut scopes: Vec<String> = route
        .api_doc
        .required_scopes
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    scopes.sort();
    scopes.dedup();
    crate::graph::MountedRoute {
        method: route.method.to_string(),
        path: scope_prefix.map_or_else(
            || route.path.to_string(),
            |prefix| crate::router::join_nested_path(prefix, route.path),
        ),
        handler: route.name.to_owned(),
        module_path: route.api_doc.module_path.to_owned(),
        // A repository auto-API route registers its guard on the *repository*,
        // not on the generated handler, so `ApiDoc` alone reports it as
        // unauthenticated. `route_listing::classify` — the derivation `autumn
        // routes audit` proves the posture with — ORs in the repository's own
        // `has_policy` and treats a registered `scope_check` as gated. The graph
        // claims to state that same posture, so it has to read the same two
        // sources; reading `ApiDoc` alone made a `#[repository(api = "...",
        // policy = ...)]` endpoint serialize as `auth: none`.
        auth: crate::graph::RouteAuth {
            secured: route.api_doc.secured,
            roles,
            scopes,
            policy: route.api_doc.has_policy
                || route
                    .repository
                    .as_ref()
                    .is_some_and(|meta| meta.has_policy),
            repository_scope: route
                .repository
                .as_ref()
                .is_some_and(|meta| meta.scope_check.is_some()),
            public: route.api_doc.public,
        },
        // Declared ownership, straight off the route the `#[repository(api =
        // "...")]` macro generated. Never re-derived from the served path.
        repository_api: route
            .repository
            .as_ref()
            .map(|meta| meta.api_path.to_owned()),
    }
}

/// The slice of a [`Route`] the agent-authority manifest needs (#1691).
///
/// Built here rather than in `agent_authority::manifest` so that module needs
/// no dependency on the router, and unconditionally rather than behind the
/// `openapi` feature: which handlers an agent can reach is not an
/// documentation concern.
///
/// `expose_all` is the app's whole-API MCP hatch. It has to be threaded in:
/// deriving tool-ness from `#[api_doc(mcp)]` alone made every route
/// `expose_all_as_mcp()` swept up invisible to this document — in neither
/// `actions` nor `ungoverned_tools` — while the document's own `excluded`
/// section claimed they surfaced there (#1691 P2-6). The same call also
/// applies the JSON-out eligibility gate, so an HTML route someone tagged
/// `#[api_doc(mcp)]` is no longer reported as a tool it will never become.
fn agent_authority_route_summary(
    route: &Route,
    scope_prefix: Option<&str>,
    expose_all: bool,
) -> crate::agent_authority::manifest::RouteSummary {
    // One string, used both for the row and for the predicate, so the two can
    // never disagree about a route's verb.
    let method = route.method.to_string();
    // The path an agent actually calls. `join_nested_path` is the same helper
    // the OpenAPI collector uses for scoped groups, so the manifest and the
    // spec cannot disagree about where a route lives.
    let path = scope_prefix.map_or_else(
        || route.path.to_string(),
        |prefix| crate::router::join_nested_path(prefix, route.path),
    );
    let exposed_by = crate::agent_authority::manifest::mcp_exposure(
        &crate::agent_authority::manifest::McpExposureInput {
            method: &method,
            hidden: route.api_doc.hidden,
            mcp_tool: route.api_doc.mcp_tool,
            mcp_exclude: route.api_doc.mcp_exclude,
            mcp_stream: route.api_doc.mcp_stream,
            has_response_schema: route.api_doc.response.is_some(),
            success_status: route.api_doc.success_status,
            expose_all,
        },
    );
    crate::agent_authority::manifest::RouteSummary {
        method,
        path,
        handler: route.name,
        // The name an MCP client actually calls: `#[api_doc(operation_id =
        // "...")]` renames the tool without renaming the handler.
        operation_id: route.api_doc.operation_id,
        module_path: route.api_doc.module_path,
        mcp_tool: exposed_by.is_some(),
        exposed_by,
        // Filled by the route macro from the handler's `#[agent_operable]`
        // marker, in either attribute order.
        agent_authority: route.api_doc.agent_authority,
    }
}

pub(crate) fn is_list_one_off_tasks_mode() -> bool {
    std::env::var("AUTUMN_LIST_TASKS").as_deref() == Ok("1")
}

/// Whether `AUTUMN_MIGRATE=1` requests the migrate-only one-shot: apply pending
/// embedded migrations and exit without starting the HTTP server. Set by
/// `autumn deploy`'s redeploy cutover (issue #1607) so migrations land before
/// traffic is flipped to the new release.
pub(crate) fn is_migrate_only_mode() -> bool {
    std::env::var("AUTUMN_MIGRATE").as_deref() == Ok("1")
}

/// The `autumn db retention` one-shot mode requested by
/// `AUTUMN_DB_RETENTION`, if any (issue #1605).
///
/// `report` counts what is eligible and deletes nothing; `purge` enforces the
/// policy immediately. Any other value is rejected loudly rather than
/// defaulting to either — guessing wrong in one direction deletes data the
/// operator did not ask to delete.
pub(crate) fn framework_retention_mode_from_env() -> Option<FrameworkRetentionMode> {
    let raw = std::env::var(FRAMEWORK_RETENTION_ENV).ok()?;
    match raw.trim() {
        "" => None,
        "report" => Some(FrameworkRetentionMode::Report),
        "purge" => Some(FrameworkRetentionMode::Purge),
        other => {
            // Warn and fall through to a normal boot rather than exiting.
            // This var is read on *every* start, so exiting here would let a
            // stray value in a wrapping script or a shared environment take a
            // production app down at boot. Mirrors `AUTUMN_ROLE`'s handling
            // of an unrecognized value.
            eprintln!(
                "Warning: {FRAMEWORK_RETENTION_ENV}={other:?} is not valid (expected \"report\" \
                 or \"purge\"), ignoring"
            );
            None
        }
    }
}

/// The env var `autumn db retention` sets to select the one-shot mode.
pub(crate) const FRAMEWORK_RETENTION_ENV: &str = "AUTUMN_DB_RETENTION";

/// The env var `autumn db retention --dataset <key>` sets to narrow the run.
pub(crate) const FRAMEWORK_RETENTION_DATASET_ENV: &str = "AUTUMN_DB_RETENTION_DATASET";

/// Line prefix framing the framework retention report on stdout, matched
/// verbatim by `autumn-cli/src/db/retention.rs`.
pub(crate) const FRAMEWORK_RETENTION_JSON_PREFIX: &str = "AUTUMN_DB_RETENTION_REPORT=";

/// What `autumn db retention` asked the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameworkRetentionMode {
    /// Count what is eligible; delete nothing.
    Report,
    /// Enforce the policy now.
    Purge,
}

/// The `--dataset` filter, if one was passed. Blank is treated as absent so a
/// wrapping script exporting an empty value cannot turn into a not-found
/// error.
pub(crate) fn framework_retention_dataset_from_env() -> Option<String> {
    std::env::var(FRAMEWORK_RETENTION_DATASET_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Whether `AUTUMN_RETENTION_DRY_RUN=1` requests the retention dry-run
/// one-shot: count (never delete) what every declared `retention(...)`
/// policy would sweep and exit. Set by `autumn retention --dry-run`
/// (issue #1342).
pub(crate) fn is_retention_dry_run_mode() -> bool {
    std::env::var("AUTUMN_RETENTION_DRY_RUN").as_deref() == Ok("1")
}

/// Line prefix framing the retention dry-run's machine-readable JSON report
/// on stdout, matched verbatim by `autumn-cli/src/retention.rs`.
///
/// Regression (#1342 review round 14): the dry-run one-shot's stdout is not
/// otherwise guaranteed to contain nothing but the JSON report — the default
/// `dev` profile initializes a stdout-backed tracing formatter, and Diesel's
/// `MigrationHarness` writes pending-migration progress directly to stdout
/// (`HarnessWithOutput::write_to_stdout`), both of which run before the
/// report line prints whenever anything is pending. Parsing the *entire*
/// captured stdout as one JSON blob (the original approach) then fails on
/// any of that incidental output. Framing the report as the one line
/// starting with this prefix lets the CLI find it regardless of what else
/// landed on stdout, without having to redirect every one-shot's logging or
/// Diesel's hardcoded migration-progress writer — both shared with other
/// boot paths (e.g. `autumn migrate`) that want stdout output.
const RETENTION_DRY_RUN_JSON_PREFIX: &str = "AUTUMN_RETENTION_DRY_RUN_REPORT=";

/// `AUTUMN_RETENTION_MODEL=<name>` narrows the dry-run report to one model's
/// policy. Set by `autumn retention --dry-run --model <name>`.
#[cfg(feature = "db")]
fn retention_dry_run_model_filter_from_env() -> Option<String> {
    std::env::var("AUTUMN_RETENTION_MODEL")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Combines `descriptors`' generated task names with `tasks` (hand-declared
/// via `tasks![...]`) and checks the merged set for a collision — the same
/// merge-then-validate step real boot performs (`tasks.extend(...)` followed
/// by `validate_unique_scheduled_task_names`, in `AppBuilder::build`) before
/// ever spawning a scheduler loop.
///
/// A free function (rather than inlined into `run_retention_dry_run_mode`)
/// so the collision case is unit-testable against synthetic descriptors and
/// tasks without booting an `AppBuilder` or touching the process-global
/// `inventory` registry (#1342 review round 18).
#[cfg(feature = "db")]
fn merge_and_validate_task_names(
    descriptors: &[&crate::retention::RetentionSweepDescriptor],
    tasks: Vec<crate::task::TaskInfo>,
) -> Result<(), String> {
    let mut merged: Vec<crate::task::TaskInfo> = descriptors
        .iter()
        .map(|descriptor| (descriptor.task_info)())
        .collect();
    merged.extend(tasks);
    crate::task::validate_unique_scheduled_task_names(&merged)
}

/// Whether `AUTUMN_REPLAY_CAPSULE=<path>` requests the capsule-replay one-shot:
/// rebuild the app offline, replay the recorded request, print the verdict and
/// exit without starting the HTTP server. Set by `autumn replay` (issue #1598).
pub(crate) fn is_replay_mode() -> bool {
    replay_capsule_from_env().is_some()
}

/// The capsule path `AUTUMN_REPLAY_CAPSULE` names, if it names one.
fn replay_capsule_from_env() -> Option<String> {
    std::env::var("AUTUMN_REPLAY_CAPSULE")
        .ok()
        .as_deref()
        .and_then(normalize_replay_capsule)
}

/// Trim a raw `AUTUMN_REPLAY_CAPSULE` value; a blank one selects no mode.
fn normalize_replay_capsule(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Force the configuration knobs a replay must not honour (F15).
///
/// A capsule is replayed on a laptop or in CI, where the recording app's Redis,
/// object storage and mail transport are not reachable — and where reaching
/// them would be a side effect of *diagnosing* a failure, not part of it. Every
/// other setting stays the application's own, because status codes, CSRF,
/// locale routing and error rendering all depend on them.
#[cfg(feature = "reporting")]
fn force_offline_replay_config(config: &mut AutumnConfig) {
    // Capturing the replay would write a capsule of a capsule, and arming
    // capture would point the connection-checkout marker at the stub pool.
    config.failure_capture.enabled = false;
    // Sessions in process memory: no Redis is dialled, and the
    // memory-in-production warning is noise for a one-shot replay.
    config.session.backend = crate::session::SessionBackend::Memory;
    config.session.allow_memory_in_production = true;
    // Channels in process memory for the same reason: the Redis backend spawns
    // a publisher and a listener task against the application's live fan-out
    // the moment the state is built.
    config.channels.backend = crate::config::ChannelBackend::InProcess;
    // Every other config-driven store the request path can reach. A replay does
    // not merely dial these, it writes to them: a rate-limit bucket is
    // decremented, an idempotency key and its lock are taken, a submit token is
    // consumed, a webhook replay key is inserted and deleted. Against the recording
    // deployment's Redis, diagnosing a failure would change production state, and
    // an unreachable backend would manufacture a 429 or 503 the recorded run never
    // produced. A replay is a read of the past; it writes nothing anywhere.
    config.security.rate_limit.backend = crate::security::config::RateLimitBackend::Memory;
    config.idempotency.backend = crate::config::IdempotencyBackend::Memory;
    config.idempotency.allow_memory_in_production = true;
    // Submit tokens inherit the idempotency backend when unset, so the
    // in-memory store is pinned explicitly rather than left to that inheritance.
    config.security.submit_token.backend = Some(crate::config::IdempotencyBackend::Memory);
    config.security.webhooks.replay.backend = crate::webhook::WebhookReplayBackend::Memory;
    config.security.webhooks.replay.allow_memory_in_production = true;
    // A `#[cached]` handler would otherwise read — and populate — the
    // application's shared cache.
    config.cache.backend = crate::config::CacheBackend::Memory;
    // A handler that enqueues a job would otherwise put it on the real queue,
    // where a live worker would run it. The replay never starts a job runtime,
    // so the enqueue lands in a process-local queue nothing drains.
    "local".clone_into(&mut config.jobs.backend);
    // No wall-clock deadline. Everything a replay consumes comes from the capsule,
    // including the clock the handler reads, but the request-timeout layer runs on
    // real tokio timers — so how long the replay takes is the one thing still
    // measured in real seconds. That matters as soon as someone attaches a
    // debugger: a breakpoint held longer than the app's `request_timeout_ms`
    // cancels the handler mid-replay and prints a mismatch that is an artefact of
    // the debugging session. A per-route `#[timeout(...)]` override still applies —
    // it is part of the route table, not the configuration.
    config.server.timeouts.request_timeout_ms = None;
}

/// The database topology a replay runs against: an in-process pool answering
/// from the capsule's recorded wire traffic, or none when the capsule recorded
/// no database work at all.
#[cfg(all(feature = "reporting", feature = "db", not(feature = "sqlite")))]
fn replay_database_topology(
    capsule: &crate::capsule::Capsule,
    divergences: &std::sync::Arc<crate::capsule::DivergenceLog>,
    capsule_path: &std::path::Path,
) -> Option<crate::db::DatabaseTopology> {
    // "This request issued no queries" and "this application has no database" are
    // different facts, and only the capsule tells them apart. A handler or state
    // initializer that checks `state.pool()`, or replica availability, before
    // querying would otherwise meet `None` while replaying an application that had
    // a pool in production, take a branch it never took, and report a mismatch no
    // code caused. A capsule recorded before `db_roles` existed carries none and
    // falls back to the old tape-only behaviour.
    if capsule.db.is_none() && capsule.db_roles.is_empty() {
        return None;
    }
    let pool = crate::capsule::pool_from_capsule(capsule, std::sync::Arc::clone(divergences))
        .unwrap_or_else(|error| {
            std::process::exit(crate::capsule::print_refusal(
                &format!("the capsule's database tape could not be served: {error}"),
                capsule_path,
            ))
        });
    // Rebuild the topology with the shape the recording had: replica-recorded
    // tapes get their own stub pool, so a write-then-read request claims each
    // tape from the role it was recorded on instead of funnelling reads into
    // the primary stub and diverging on both sides.
    let replica =
        crate::capsule::replica_pool_from_capsule(capsule, std::sync::Arc::clone(divergences))
            .unwrap_or_else(|error| {
                std::process::exit(crate::capsule::print_refusal(
                    &format!("the capsule's replica tape could not be served: {error}"),
                    capsule_path,
                ))
            });
    Some(crate::db::DatabaseTopology::from_pools(pool, replica))
}

/// `SQLite` builds have no wire capture and no wire replay (F18), so a capsule
/// carrying a database tape — which is `PostgreSQL` protocol traffic — cannot be
/// replayed by this binary.
#[cfg(all(feature = "reporting", feature = "db", feature = "sqlite"))]
fn replay_database_topology(
    capsule: &crate::capsule::Capsule,
    _divergences: &std::sync::Arc<crate::capsule::DivergenceLog>,
    capsule_path: &std::path::Path,
) -> Option<crate::db::DatabaseTopology> {
    if capsule.db.is_some() {
        std::process::exit(crate::capsule::print_refusal(
            "the capsule carries a PostgreSQL database tape, but this binary was built with the \
             `sqlite` backend, which has neither wire capture nor wire replay. Replay it with a \
             PostgreSQL build of the application.",
            capsule_path,
        ));
    }
    None
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
                        state.task_registry.record_next_run_at(
                            &name,
                            &format_next_task_run_after(state.clock().now(), delay),
                        );
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
            "timestamp": state.clock().now().to_rfc3339(),
        });
        if let Some(map) = msg.as_object_mut() {
            for (k, v) in extra {
                map.insert(k.to_string(), v);
            }
        }
        let _ = state.channels().sender("sys:tasks").send(msg.to_string());
    }
}

/// Milliseconds elapsed since `start` on the app's injected monotonic clock.
///
/// Every scheduled-task duration goes through here so a `#[sim_test]` reports
/// virtual run times (and two same-seed runs agree) instead of real ones.
fn task_duration_ms(state: &AppState, start: crate::time::MonotonicInstant) -> u64 {
    let elapsed = state.monotonic().saturating_duration_since(start);
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

async fn execute_task_result(
    state: &AppState,
    handler: crate::task::TaskHandler,
    start: crate::time::MonotonicInstant,
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
            let duration_ms = task_duration_ms(state, start);
            return Err((duration_ms, format_scheduled_task_panic(panic.as_ref())));
        }
    };
    let result = std::panic::AssertUnwindSafe(future).catch_unwind().await;
    let duration_ms = task_duration_ms(state, start);

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
    start: crate::time::MonotonicInstant,
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
        let duration_ms = task_duration_ms(state, start);
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

    let start = state.monotonic();
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
            crate::alerts::notify_scheduled_task_recovered(&state, &name);
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
            crate::alerts::notify_scheduled_task_failure(&state, &name, &error_str);
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

    let start = state.monotonic();
    let lease_ttl = lease_ttl_for_run(&lease, coordination, lease_ttl);
    match execute_task_result_with_optional_lease_ttl(
        &state, handler, start, &name, "cron", lease_ttl,
    )
    .await
    {
        Ok(duration_ms) => {
            state.task_registry.record_success(&name, duration_ms);
            crate::alerts::notify_scheduled_task_recovered(&state, &name);
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
            crate::alerts::notify_scheduled_task_failure(&state, &name, &error_str);
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
    let mut cursor = state.clock().now().with_timezone(&timezone);

    loop {
        let now = state.clock().now().with_timezone(&timezone);
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
        let sleep_for = cron_sleep_duration_until(state.clock().now(), &scheduled_at);
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(sleep_for) => {
                let woke_at = state.clock().now().with_timezone(&timezone);
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

/// Render the next fixed-delay run time, `delay` after `now`.
///
/// `now` is passed in (rather than read here) so the caller supplies it from the
/// app's injected clock and the rendered `next_run_at` follows virtual time
/// under a `#[sim_test]`.
///
/// Goes through [`crate::job::due_at_from`] for the addition rather than `now +
/// delay`. Once `now` comes from an injected clock it is no longer bounded by
/// real time, and chrono's `Add` **panics** on overflow — a clock pinned near
/// `DateTime::MAX_UTC` would kill this scheduler task before its first run, for
/// a value that only ends up in a log line. `due_at_from` already owns that
/// clamp for the job queue's deadlines; sharing it keeps the two from drifting
/// apart, and makes an unrepresentable delay render as the far future rather
/// than as `now` (which would read as "runs immediately").
fn format_next_task_run_after(
    now: chrono::DateTime<chrono::Utc>,
    delay: std::time::Duration,
) -> String {
    crate::job::due_at_from(now, delay).to_rfc3339()
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

/// How long to sleep from `now` until `scheduled_at`, saturating at zero for a
/// target already in the past.
///
/// `now` is passed in so the caller supplies it from the app's injected clock.
/// Note the caller must keep this in step with `tokio::time::sleep`: under a
/// `#[sim_test]` both the injected clock and tokio's timer wheel are advanced
/// together by `Sim::advance`, which is what keeps the loop from spinning.
fn cron_sleep_duration_until<Tz: chrono::TimeZone>(
    now: chrono::DateTime<chrono::Utc>,
    scheduled_at: &chrono::DateTime<Tz>,
) -> std::time::Duration {
    scheduled_at
        .with_timezone(&chrono::Utc)
        .signed_duration_since(now)
        .to_std()
        .unwrap_or_default()
}

async fn run_startup_hooks(hooks: &[StartupHook], state: AppState) -> crate::AutumnResult<()> {
    for hook in hooks {
        hook(state.clone()).await?;
    }
    Ok(())
}

/// Install a designated live-state block into `state`, adopting the snapshot a
/// predecessor handed over when this process was started by an in-place
/// upgrade (issue #1674).
///
/// A snapshot this build cannot account for is a hard startup failure, not a
/// silent fallback to `initial`: the predecessor is still serving and still
/// holds the only copy of that state, so exiting here abandons the upgrade and
/// keeps the data. On a cold start there is no snapshot and `initial` is used.
fn install_live_state<T>(
    state: &AppState,
    initial: T,
    decode: fn(&crate::upgrade::StateEnvelope) -> Result<T, crate::upgrade::AdoptError>,
) where
    T: crate::upgrade::LiveState,
{
    if let Some(existing) = state.extension::<crate::upgrade::LiveStateRegistry>() {
        state.insert_extension(crate::upgrade::LiveStateInstallFailure(format!(
            "an app may designate only one block of live state for in-place upgrades, but \
             both {} and {} were designated; carrying just one of two designated blocks \
             would be the silent state loss this feature exists to prevent",
            existing.type_name(),
            std::any::type_name::<T>(),
        )));
        return;
    }

    let value = match crate::upgrade::carried_snapshot_path() {
        None => initial,
        Some(path) => {
            match crate::upgrade::read_snapshot(&path).and_then(|envelope| {
                let decoded = decode(&envelope);
                if decoded.is_ok() {
                    tracing::info!(
                        state = std::any::type_name::<T>(),
                        from_version = envelope.version,
                        to_version = T::VERSION,
                        generation = envelope.generation,
                        "adopted the live state handed over by the previous build"
                    );
                }
                decoded
            }) {
                Ok(value) => value,
                Err(error) => {
                    state.insert_extension(crate::upgrade::LiveStateInstallFailure(format!(
                        "the previous build's live state cannot be carried into this one \
                         ({}): {error}. The previous build keeps serving; fix the \
                         migration (autumn_web::state_migration!) and try the upgrade again",
                        std::any::type_name::<T>(),
                    )));
                    return;
                }
            }
        }
    };

    // A successor installs its state **frozen**. It starts accepting the moment
    // it adopts the socket — before its startup hooks have run — and until it
    // signals readiness the upgrade can still be abandoned, at which point the
    // predecessor resumes from the snapshot it took. A write acknowledged in
    // that window would die with this process: refuse it instead, so the
    // client's retry lands on whichever process is actually keeping state.
    // `unfreeze_adopted_live_state` lifts it at the readiness point.
    let handle = if crate::upgrade::handoff_requested() {
        crate::upgrade::LiveStateHandle::new_frozen(value)
    } else {
        crate::upgrade::LiveStateHandle::new(value)
    };
    state.insert_extension(crate::upgrade::LiveStateRegistry::new(&handle));
    state.insert_extension(handle);
}

/// Make an adopted live-state block writable, once this build has finished
/// starting up and is about to release its predecessor (#1674).
///
/// A no-op on a cold start (the block was never frozen) and for an app that
/// designated none.
fn unfreeze_adopted_live_state(state: &AppState) {
    if !crate::upgrade::handoff_requested() {
        return;
    }
    if let Some(registry) = state.extension::<crate::upgrade::LiveStateRegistry>() {
        registry.unfreeze();
    }
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
/// Build the [`EventRegistry`](crate::events::EventRegistry) from `listeners` and
/// append the synthesized `default`-queue [`JobInfo`](crate::job::JobInfo) for
/// each durable listener to `jobs`, returning the registry.
///
/// This is the pure, DB-free half of [`finalize_event_bus`]: it needs no live
/// `AppState` or database, only the listener set. Both the boot path (through
/// `finalize_event_bus`, which additionally wires the global bus onto live state)
/// and the deliberately DB-free `AUTUMN_DUMP_JOBS=1` dump path
/// ([`dump_jobs_manifest`]) funnel through here, so the emitted manifest can
/// never omit the durable-listener jobs the runtime actually drains.
fn synthesize_durable_listener_jobs(
    listeners: Vec<crate::events::ListenerInfo>,
    jobs: &mut Vec<crate::job::JobInfo>,
) -> crate::events::EventRegistry {
    let registry = crate::events::EventRegistry::from_listeners(listeners);
    jobs.extend(registry.durable_job_infos());
    registry
}

fn finalize_event_bus(
    listeners: Vec<crate::events::ListenerInfo>,
    jobs: &mut Vec<crate::job::JobInfo>,
    state: &AppState,
) {
    let registry = synthesize_durable_listener_jobs(listeners, jobs);
    state.insert_extension(registry.clone());
    crate::events::init_global_event_bus(&registry, state, None);
}

/// Compute the `AUTUMN_DUMP_JOBS=1` jobs manifest for the dump path.
///
/// Mirrors the boot path's job set: the builder's registered `jobs` PLUS the
/// synthesized `default`-queue jobs that [`finalize_event_bus`] appends for
/// durable listeners before the runtime starts. Without folding those in, an app
/// that registers a durable listener and configures `[jobs.queues]` without
/// `default` would emit a manifest omitting `default`, letting a topology-aware
/// `autumn doctor` accept a fleet where no tier drains the durable-listener jobs.
///
/// Only the pure job-synthesis half of `finalize_event_bus` runs here
/// (via [`synthesize_durable_listener_jobs`]); the dump path is deliberately
/// DB-free, so the live-state/global-bus wiring is skipped. Factored out so the
/// boot and dump paths share one job-preparation seam and so it is unit testable
/// without the process-exiting dump entrypoint.
fn dump_jobs_manifest(
    cfg: &crate::config::JobQueuesConfig,
    mut jobs: Vec<crate::job::JobInfo>,
    listeners: Vec<crate::events::ListenerInfo>,
) -> String {
    synthesize_durable_listener_jobs(listeners, &mut jobs);
    crate::job::render_jobs_manifest(cfg, &jobs)
}

fn initialize_job_runtime(
    jobs: Vec<crate::job::JobInfo>,
    state: &AppState,
    shutdown: &tokio_util::sync::CancellationToken,
    config: &crate::config::JobConfig,
    run_workers: bool,
) -> crate::AutumnResult<()> {
    crate::job::clear_global_job_client();
    if jobs.is_empty() {
        Ok(())
    } else {
        crate::job::start_runtime(jobs, state, shutdown, config, run_workers)
    }
}

/// Bind the configured TCP address — or adopt the listening socket a
/// predecessor handed over during an in-place upgrade (issue #1674).
///
/// Adopting is what makes the cutover connectionless-loss-free: the socket is
/// never closed and re-opened, so a connection queued on it while both builds
/// are alive is served by whichever one accepts it. Exits (rather than falling
/// back to `bind`) if an inherited socket turns out to be unusable: the
/// predecessor still holds the real one and is still serving, so a successor
/// that cannot adopt must abandon the upgrade, not race it for the port.
async fn bind_or_adopt_tcp_listener(addr: &str) -> tokio::net::TcpListener {
    #[cfg(unix)]
    {
        if let Some(inherited) = crate::upgrade::adopt_inherited_listener() {
            match tokio::net::TcpListener::from_std(inherited) {
                Ok(listener) => {
                    // The socket is the predecessor's, so a `server.host` /
                    // `server.port` change in the new build does NOT take
                    // effect: say so rather than let an operator believe a
                    // narrowed bind address is live.
                    if let Ok(bound) = listener.local_addr()
                        && bound.to_string() != addr
                    {
                        tracing::warn!(
                            inherited = %bound,
                            configured = %addr,
                            "the inherited listening socket does not match this build's \
                             configured address; an in-place upgrade cannot change where the \
                             app listens — restart the process to apply it"
                        );
                    }
                    return listener;
                }
                Err(e) => {
                    tracing::error!("Failed to adopt the inherited listening socket: {e}");
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(1);
                }
            }
        }
        // A predecessor handed us a socket and we could not take it: binding
        // instead would either collide with the process still serving that
        // address, or — worse, with an ephemeral port — succeed somewhere else
        // entirely and release the predecessor to drain away from the address
        // clients are using.
        if crate::upgrade::handoff_requested() {
            tracing::error!(
                "refusing to start: this build was handed the previous build's listening \
                 socket but could not adopt it (see the error above). The previous build \
                 keeps serving"
            );
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        }
    }
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(addr = %addr, "Failed to bind: {e}");
            // Stop the managed Postgres child started by `setup_database`
            // before bailing; `process::exit` skips `on_shutdown`.
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        }
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
    /// TLS-terminating listener on `host:port` (direct HTTPS, issue #1603).
    #[cfg(feature = "tls")]
    Tls(crate::tls::TlsListener),
}

/// Bind a TLS-terminating listener over `tcp`, loading and validating the
/// configured certificate and key (fail-fast on any problem).
#[cfg(feature = "tls")]
fn build_tls_listener(
    tcp: tokio::net::TcpListener,
    cfg: &crate::config::TlsConfig,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<(crate::tls::TlsListener, crate::tls::CertReloader), crate::tls::TlsError> {
    let provider = crate::tls::crypto_provider();
    // The pre-bind `TlsConfig::validate()` guarantees both paths are set in
    // static-cert mode (the only mode that reaches this function; ACME mode is
    // handled separately), so unwrapping here is validated, not hopeful.
    let cert_path = cfg
        .cert_path
        .as_deref()
        .expect("validated: static [server.tls] sets cert_path");
    let key_path = cfg
        .key_path
        .as_deref()
        .expect("validated: static [server.tls] sets key_path");
    // One call, so the reload baseline is stat'd before the certificate is
    // loaded: a renewal landing in that gap must read as a change on the next
    // poll, not as the baseline (which would serve the superseded certificate
    // until the following renewal).
    let (resolver, reload) = crate::tls::CertReloader::load(
        cert_path.to_path_buf(),
        key_path.to_path_buf(),
        std::sync::Arc::clone(&provider),
        crate::tls::now_unix(),
        // A zero interval would busy-loop; clamp to at least one second.
        std::time::Duration::from_secs(cfg.reload_interval_secs.max(1)),
    )?;
    let server_config = crate::tls::build_server_config(
        std::sync::Arc::clone(&provider),
        std::sync::Arc::clone(&resolver),
    )?;
    // A zero handshake timeout would drop every connection instantly; clamp to
    // at least one second, mirroring the reload-interval clamp above.
    let handshake_timeout = std::time::Duration::from_secs(cfg.handshake_timeout_secs.max(1));
    let listener = crate::tls::TlsListener::new(tcp, server_config, handshake_timeout, shutdown);
    Ok((listener, reload))
}

/// Carries the ACME challenge-listener + renewal-task wiring from the bind path
/// to the sibling tasks spawned once `server_shutdown` exists.
#[cfg(feature = "acme")]
struct AcmeBindState {
    renewal_task: crate::acme::renewal::AcmeRenewalTask,
    tokens: crate::acme::challenge::Http01Tokens,
    http_challenge_port: u16,
    https_port: u16,
    /// Whether DNS-01 is configured. Decides whether a failure to bind the
    /// challenge/redirect port is fatal (HTTP-01) or a warning (DNS-01, where
    /// the CA never connects to this host).
    dns01: bool,
}

/// Build a TLS listener for ACME mode: serve a stored certificate if one is
/// present, else a self-signed placeholder so `:443` binds immediately. The
/// returned [`AcmeBindState`] carries everything the renewal task and challenge
/// listener need.
#[cfg(feature = "acme")]
async fn build_acme_tls_listener(
    tcp: tokio::net::TcpListener,
    tls_cfg: &crate::config::TlsConfig,
    acme_cfg: &crate::config::AcmeConfig,
    credentials: &crate::credentials::CredentialsStore,
    https_port: u16,
    status: Option<crate::acme::renewal::AcmeStatus>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<(crate::tls::TlsListener, AcmeBindState), String> {
    use crate::acme::store::{AcmeStore, CertId, FsAcmeStore};

    let provider = crate::tls::crypto_provider();
    let cert_id = CertId::from_domains(&acme_cfg.domains);
    let directory_label = crate::acme::directory_label(&acme_cfg.directory);
    let store: std::sync::Arc<dyn AcmeStore> = std::sync::Arc::new(FsAcmeStore::new(
        acme_cfg.cache_dir.clone(),
        directory_label,
    ));
    let status = status.unwrap_or_default();

    // Prefer a valid stored certificate; fall back to a self-signed placeholder
    // so the port comes up while the first issuance runs in the background.
    let (initial, serving_stored_cert) = match store.load_cert(&cert_id).await {
        Ok(Some(stored)) => match crate::tls::certified_key_from_pem(
            stored.chain_pem.as_bytes(),
            stored.key_pem.as_bytes(),
            &provider,
        ) {
            Ok(ck) => {
                if let Ok(not_after) =
                    crate::tls::leaf_not_after_from_pem(stored.chain_pem.as_bytes())
                {
                    status.set_cert_not_after(not_after);
                }
                (ck, true)
            }
            Err(e) => {
                tracing::warn!(
                    "stored ACME certificate is unusable ({e}); serving a self-signed \
                     placeholder until the renewal task issues a real one"
                );
                (acme_placeholder_key(&acme_cfg.domains, &provider)?, false)
            }
        },
        Ok(None) => (acme_placeholder_key(&acme_cfg.domains, &provider)?, false),
        Err(e) => {
            tracing::warn!(
                "failed to read the stored ACME certificate ({e}); serving a self-signed \
                 placeholder"
            );
            (acme_placeholder_key(&acme_cfg.domains, &provider)?, false)
        }
    };

    let resolver = std::sync::Arc::new(crate::tls::ReloadableCertResolver::new(initial));
    let server_config = crate::tls::build_server_config(
        std::sync::Arc::clone(&provider),
        std::sync::Arc::clone(&resolver),
    )
    .map_err(|e| e.to_string())?;
    let handshake_timeout = std::time::Duration::from_secs(tls_cfg.handshake_timeout_secs.max(1));
    let listener = crate::tls::TlsListener::new(tcp, server_config, handshake_timeout, shutdown);

    let tokens = crate::acme::challenge::Http01Tokens::new();
    // `[server.tls.acme.dns]` selects DNS-01, the only challenge a CA will
    // validate for a wildcard identifier (#1620). Built here, at bind time, so a
    // missing or malformed provider credential fails startup with an actionable
    // message instead of surfacing as a failed order 30 days later.
    let dns = build_dns_challenge(acme_cfg, credentials)?;
    let renewal_task = crate::acme::renewal::AcmeRenewalTask {
        resolver,
        provider,
        store,
        cert_id,
        tokens: tokens.clone(),
        status,
        config: acme_cfg.clone(),
        serving_stored_cert,
        // Filled in at the renewal spawn site once the scheduler coordinator has
        // been built and any distributed → in-process fallback is known.
        leadership_degraded: false,
        renew_window_misconfigured: std::sync::atomic::AtomicBool::new(false),
        dns,
        // Filled in at the renewal spawn site, where `AppState` (and so the
        // operator alerter) is in scope.
        recovery: None,
    };
    Ok((
        listener,
        AcmeBindState {
            renewal_task,
            tokens,
            http_challenge_port: acme_cfg.http_challenge_port,
            https_port,
            dns01: acme_cfg.dns.is_some(),
        },
    ))
}

/// The multi-replica ACME warning for this deployment, or `None` when the
/// scheduler backend says the deployment is single-replica.
///
/// One condition guards two distinct hazards, and DNS-01 only removes the
/// first:
///
/// 1. The HTTP-01 token map is per-process, so behind a load balancer the CA's
///    `:80` request can land on a replica that never minted the token (404).
/// 2. The certificate store is local disk, so replicas that did not win the
///    renewal lease never see the issued certificate and keep serving the
///    self-signed placeholder.
///
/// DNS-01 proves control through a TXT record, which retires (1) — but it does
/// not distribute certificates, so (2) still mis-serves TLS on every replica
/// but the leader. Warn either way; only the text differs (issue #1620).
///
/// Keyed off the *configured* backend (operator intent) rather than the built
/// coordinator, so the warning still fires when `coordinator_from_config` fell
/// back to in-process after a Postgres error — exactly the case where the fleet
/// is multi-replica but this process degraded. The exhaustive `matches!` is
/// compiler-enforced if a new distributed backend variant is added.
#[cfg(feature = "acme")]
const fn acme_fleet_warning(
    backend: crate::config::SchedulerBackend,
    dns01: bool,
) -> Option<&'static str> {
    if !backend.is_fleet_distributed() {
        return None;
    }
    Some(if dns01 {
        "ACME DNS-01 issuance is fleet-safe, but the on-disk certificate store is not: only \
         the replica holding the renewal lease writes the issued certificate, and the others \
         cannot adopt it from a non-shared store, so they keep serving the self-signed \
         placeholder. Run ACME on a single host, or point [server.tls.acme] cache_dir at \
         storage every replica shares (#1620)"
    } else {
        "ACME HTTP-01 validation is not fleet-safe with the local on-disk token store: behind \
         a load balancer the CA's :80 challenge may reach a replica without the token (404), \
         and non-leader replicas cannot adopt issued certificates from a non-shared store. Run \
         ACME on a single host, or terminate TLS at a shared proxy. DNS-01 removes the :80 \
         hazard but not the store one, so it needs a shared [server.tls.acme] cache_dir too \
         (#1620)"
    })
}

/// Build the DNS-01 challenge wiring for `[server.tls.acme.dns]`, if configured.
///
/// The provider credential is read from the encrypted credentials store (or the
/// documented `AUTUMN_ACME_DNS_*` environment variables) — never from
/// `autumn.toml`, which has no field that could hold one. A missing or blank
/// credential is an error **here**, at bind time, rather than a failed order
/// discovered when the certificate is already near expiry (issue #1620).
#[cfg(feature = "acme")]
fn build_dns_challenge(
    acme_cfg: &crate::config::AcmeConfig,
    credentials: &crate::credentials::CredentialsStore,
) -> Result<Option<crate::acme::renewal::DnsChallenge>, String> {
    use crate::acme::dns;

    let Some(dns_cfg) = acme_cfg.dns.as_ref() else {
        return Ok(None);
    };
    let resolvers = dns_cfg.resolver_addrs()?;
    let credential = dns::DnsCredential::resolve(dns_cfg, credentials, &dns::process_env);
    // One bounded HTTP timeout for the provider API and one for each DNS probe,
    // so neither can park the renewal loop: the whole propagation wait is
    // already bounded, and a black-holed provider API must not outlive it.
    let transport: std::sync::Arc<dyn dns::http::HttpTransport> = std::sync::Arc::new(
        dns::http::ReqwestTransport::new(std::time::Duration::from_secs(30))?,
    );
    let provider = dns::build_provider(dns_cfg, &credential, transport)?;
    Ok(Some(crate::acme::renewal::DnsChallenge {
        provider,
        lookup: std::sync::Arc::new(dns::resolver::UdpDnsLookup::new(
            std::time::Duration::from_secs(5),
        )),
        resolvers,
        propagation_timeout: std::time::Duration::from_secs(dns_cfg.propagation_timeout_secs),
        poll_interval: std::time::Duration::from_secs(dns_cfg.poll_interval_secs),
    }))
}

/// Build a `CertifiedKey` from a fresh self-signed placeholder for `domains`.
#[cfg(feature = "acme")]
fn acme_placeholder_key(
    domains: &[String],
    provider: &rustls::crypto::CryptoProvider,
) -> Result<std::sync::Arc<rustls::sign::CertifiedKey>, String> {
    let placeholder = crate::acme::renewal::self_signed_placeholder(domains)?;
    crate::tls::certified_key_from_pem(
        placeholder.chain_pem.as_bytes(),
        placeholder.key_pem.as_bytes(),
        provider,
    )
}

/// Build the ACME renewal failure reporter: dispatch each failure as an
/// [`ErrorEvent`](crate::reporting::ErrorEvent) to the registered reporter chain
/// on a detached task, so failures reach Sentry/etc. when configured (failures
/// also always log via `tracing` inside the loop).
#[cfg(all(feature = "acme", feature = "reporting"))]
fn make_acme_reporter(
    reporters: Vec<std::sync::Arc<dyn crate::reporting::ErrorReporter>>,
) -> crate::acme::renewal::ReporterFn {
    std::sync::Arc::new(move |message: String| {
        if reporters.is_empty() {
            return;
        }
        let reporters = reporters.clone();
        tokio::spawn(async move {
            let event = crate::reporting::ErrorEvent {
                status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                message,
                problem_type: None,
                request_id: None,
                route: Some("acme-renewal".to_owned()),
                method: None,
                panic: None,
                capsule: None,
            };
            for reporter in &reporters {
                reporter.report(&event).await;
            }
        });
    })
}

/// The scheduled-operation name ACME renewal alerts are keyed on. Stable: the
/// trigger and its recovery must share it, or the recovery cannot clear the
/// outstanding alert.
#[cfg(feature = "acme")]
const ACME_RENEWAL_TASK_NAME: &str = "acme-renewal";

/// Wrap `inner` so every ACME failure ALSO raises #1610's
/// `scheduled_task_failure` operator alert.
///
/// Composition rather than replacement: the error-reporting chain (Sentry et al)
/// still sees the failure, and the alerter is an independent destination an
/// operator actually watches.
#[cfg(feature = "acme")]
fn compose_acme_alert_reporter(
    inner: crate::acme::renewal::ReporterFn,
    state: &AppState,
) -> crate::acme::renewal::ReporterFn {
    let state = state.clone();
    std::sync::Arc::new(move |message: String| {
        crate::alerts::notify_scheduled_task_failure(&state, ACME_RENEWAL_TASK_NAME, &message);
        inner(message);
    })
}

/// The callback that clears an outstanding ACME renewal alert once issuance
/// succeeds again.
#[cfg(feature = "acme")]
fn make_acme_alert_recovery(state: &AppState) -> crate::acme::renewal::RecoveryFn {
    let state = state.clone();
    std::sync::Arc::new(move || {
        crate::alerts::notify_scheduled_task_recovered(&state, ACME_RENEWAL_TASK_NAME);
    })
}

/// The no-op ACME reporter used when the `reporting` feature is off (failures
/// still log via `tracing`).
#[cfg(all(feature = "acme", not(feature = "reporting")))]
fn make_acme_reporter() -> crate::acme::renewal::ReporterFn {
    std::sync::Arc::new(|_message: String| {})
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

/// How long to wait for the cluster's departure notice, given how much of the
/// shutdown budget the request drain already spent.
///
/// The departure itself is bounded by `LEAVE_BUDGET` inside the node; this
/// clamps the *wait* to what is left of `shutdown_timeout_secs`, so the leave
/// notice is a slice of the shutdown budget rather than an extension of it.
/// Without the clamp a drain that ran to its deadline would still be followed
/// by a full `LEAVE_BUDGET` sleep, and a supervisor timing the process out at
/// `shutdown_timeout_secs` could `SIGKILL` it mid-hook.
///
/// Returns zero when the drain has already consumed the budget: the departure
/// is skipped and the peer converges on the suspicion timeout, which is the
/// documented contract for a shutdown that overran.
fn cluster_departure_wait(
    shutdown_budget: std::time::Duration,
    drain_elapsed: std::time::Duration,
) -> std::time::Duration {
    crate::cluster::LEAVE_BUDGET.min(shutdown_budget.saturating_sub(drain_elapsed))
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
    let deadline = tokio::time::Instant::now() + total_budget;
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
/// `build_session_layer` — and by then, `setup_database` has already run
/// migrations, leaving DB side effects from a doomed boot. This helper
/// runs the same `backend_plan` check `build_session_layer` does, but
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
    plugin_config_roots: BTreeSet<String>,
) -> (AutumnConfig, crate::telemetry::TelemetryGuard) {
    let config = load_config_only(config_loader, plugin_config_roots).await;

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

/// Resolve the effective configuration WITHOUT initializing telemetry.
///
/// Split out of [`load_config_and_telemetry`] for the one-shot dump modes that
/// need config but must not touch the outside world. A custom
/// `TelemetryProvider::init` may open a collector connection, read credentials
/// or otherwise reach production resources, and telemetry cannot influence what
/// those modes emit — so `autumn openapi export`, advertised as binding no port
/// and opening no database, must not trigger it either (issue #802).
///
/// Everything up to and including [`AutumnConfig::apply_retention_caps`] is
/// shared with the telemetry-initializing path, so the config the two resolve is
/// identical.
async fn load_config_only(
    config_loader: Option<ConfigLoaderFactory>,
    plugin_config_roots: BTreeSet<String>,
) -> AutumnConfig {
    // 1. Load configuration via the installed loader, falling back to the
    //    five-layer TOML + env default.
    //
    // A custom `config_loader` factory owns its whole load and strict-config
    // handling, bypassing the default TOML path, so it does not receive the
    // declared plugin config roots; it accepts its own plugin-owned sections. The
    // default `TomlEnvConfigLoader` does get the roots, so `server.strict_config`
    // treats a plugin-declared `[root]` such as `[media]` as known-and-opaque
    // rather than an unknown-key hard error.
    let mut config = match config_loader {
        Some(factory) => factory().await,
        None => {
            crate::config::TomlEnvConfigLoader::new()
                .with_plugin_config_roots(plugin_config_roots)
                .load()
                .await
        }
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

    // #1605: tighten every TTL-native subsystem knob to its `[retention]`
    // window before anything is built from the config, so the cap flows into
    // the idempotency layer's TTL, the session cookie's Max-Age and Redis
    // TTL, and `autumn_job_tracking.expires_at` without each of those sites
    // having to know the policy exists. A pure `min`, so a config with no
    // `[retention]` section is untouched.
    config.apply_retention_caps();

    config
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

/// Excludes every `#[static_get]` route from locale-prefix routing (issue
/// #1251).
///
/// `#[static_get]` pre-rendering (`autumn build`, and ISR re-renders) is not
/// locale-aware: it requests each `StaticRouteMeta::path` once and writes the
/// single response it gets back to disk. Locale-prefixing that same path
/// would replace its content with a bare-path 308 redirect (the locale
/// segment is only known once nested under `/{locale}`), and
/// `render_static_routes` treats any non-2xx response as a build failure —
/// so without this exclusion, enabling `locale_prefix_enabled` breaks
/// `autumn build` for every app with a static route.
///
/// Static routes therefore keep serving at their single, unprefixed path
/// (matching pre-#1251 behavior) even when locale-prefix routing is on for
/// the rest of the app. Full per-locale static generation is a natural
/// follow-up, not attempted here.
///
/// Populates [`I18nConfig::locale_prefix_exclude_exact`](crate::i18n::I18nConfig::locale_prefix_exclude_exact),
/// not [`I18nConfig::locale_prefix_exclude`](crate::i18n::I18nConfig::locale_prefix_exclude):
/// a static route is a single literal path, not a namespace, so it must be
/// excluded by exact match — otherwise a static `/posts` would, as a
/// *prefix*, also swallow an unrelated dynamic sibling like `/posts/{slug}`
/// (Codex review).
#[cfg(feature = "i18n")]
fn exclude_static_routes_from_locale_prefix(
    config: &mut AutumnConfig,
    static_metas: &[crate::static_gen::StaticRouteMeta],
) {
    if config.i18n.locale_prefix_enabled {
        config
            .i18n
            .locale_prefix_exclude_exact
            .extend(static_metas.iter().map(|meta| meta.path.to_owned()));
    }
}

#[cfg(not(feature = "i18n"))]
const fn exclude_static_routes_from_locale_prefix(
    _config: &mut AutumnConfig,
    _static_metas: &[crate::static_gen::StaticRouteMeta],
) {
}

/// Derives the sitemap's locale-prefix config (issue #1251) from
/// `[i18n] locale_prefix_enabled`. `None` when the feature is off, disabled,
/// or the `i18n` cargo feature isn't compiled in — the sitemap then lists a
/// single unprefixed URL per static path, exactly as before this feature.
#[cfg(feature = "i18n")]
fn sitemap_locale_config(config: &AutumnConfig) -> Option<crate::seo::SitemapLocaleConfig<'_>> {
    config
        .i18n
        .locale_prefix_enabled
        .then_some(crate::seo::SitemapLocaleConfig {
            supported_locales: &config.i18n.supported_locales,
            exclude_prefixes: &config.i18n.locale_prefix_exclude,
            exclude_exact: &config.i18n.locale_prefix_exclude_exact,
        })
}

#[cfg(not(feature = "i18n"))]
const fn sitemap_locale_config(
    _config: &AutumnConfig,
) -> Option<crate::seo::SitemapLocaleConfig<'_>> {
    None
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
    i18n: &crate::i18n::I18nConfig,
) -> Vec<CustomLayerRegistration> {
    // #1384: install the resolution defaults from config first, before the
    // no-bundle early return. `locale_prefix_enabled` works without
    // `.i18n()`/`.i18n_auto()` — the router builds its nests straight from
    // `I18nConfig` — and in that shape no `Bundle` exists, but column decoding
    // still needs the app's default locale. Without this, a `default_locale = "fr"`
    // app attributed every legacy plain-text value to the last-resort "en", so a
    // `/fr/...` request rendered upgraded content as empty and a later write could
    // persist it under the wrong locale. A bundle, when present, re-installs the
    // same values below: it derives them from this same `I18nConfig`.
    crate::i18n::install_locale_defaults(&i18n.default_locale, i18n.resolved_fallback_chain());

    let Some(bundle) = bundle else {
        return custom_layers;
    };

    tracing::info!(
        locales = ?bundle.locales(),
        default = bundle.default_locale(),
        "i18n bundle loaded"
    );
    // #1384: content resolution outside a request (a job worker, a scheduled
    // task, a CLI command) has no locale scope to read a chain from. Publish
    // the bundle's resolved chain — the same one UI strings walk — as the
    // process default so `Translated::resolve` behaves identically there.
    crate::i18n::install_locale_defaults(bundle.default_locale(), bundle.fallback_chain().to_vec());
    state.insert_extension::<Arc<crate::i18n::Bundle>>(bundle.clone());
    // Use the existing IntoAppLayer plumbing so the Extension is visible to
    // every request. axum::Extension<T> is itself a tower::Layer when T:
    // Clone + Send + Sync + 'static.
    let ambient_layer = crate::i18n::AmbientLocaleLayer::new(&bundle);
    let ext_layer = axum::Extension(bundle);
    custom_layers.push(CustomLayerRegistration {
        type_id: TypeId::of::<axum::Extension<Arc<crate::i18n::Bundle>>>(),
        type_name: std::any::type_name::<axum::Extension<Arc<crate::i18n::Bundle>>>(),
        layer: tower::util::BoxCloneSyncServiceLayer::new(ext_layer),
    });
    // #1384: publish the negotiated locale as the ambient one for the whole
    // handler, so a `#[translatable]` field resolves itself with no locale
    // argument in the signature. Registered AFTER the bundle Extension —
    // registration order is outermost-first, so this layer runs INSIDE it and
    // its `Locale` extraction can see the bundle it needs to negotiate against.
    custom_layers.push(CustomLayerRegistration {
        type_id: TypeId::of::<crate::i18n::AmbientLocaleLayer>(),
        type_name: std::any::type_name::<crate::i18n::AmbientLocaleLayer>(),
        layer: tower::util::BoxCloneSyncServiceLayer::new(ambient_layer),
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
                    // Directory routing is active but there is no control URL for
                    // a dedicated LISTEN connection — a custom
                    // `DatabasePoolProvider` supplied the control pool without
                    // `database.primary_url`/`url`. The router still serves lookups
                    // from that pool, but re-pins are not invalidated fleet-wide on
                    // commit; they take effect only after the cache TTL expires.
                    // Warn rather than fall back silently, so an operator relying
                    // on the directory for slot moves configures a control URL, or
                    // accepts TTL-only refresh, deliberately.
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
            // A custom shard provider established shard pools without going
            // through the built-in `create_shard_topology` factory, which
            // validates `database.statement_timeout` internally, so enforce the
            // same fail-closed guard here — but only now that pools were really
            // established. `resolve_shard_set` already returned early for a
            // shardless profile, so this Some-gated check never rejects a
            // no-database path. A shard set establishing SQLite pools under a
            // nonzero timeout still fails closed.
            #[cfg(feature = "sqlite")]
            crate::db::reject_sqlite_statement_timeout(config.database.statement_timeout)
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
// The `sqlite` feature adds a small fail-fast startup-migration guard block
// (issue #1614) that pushes this orchestration fn just over the line limit.
#[allow(clippy::too_many_lines)]
async fn setup_database(
    config: &AutumnConfig,
    migrations: Vec<(&'static str, crate::migrate::EmbeddedMigrations)>,
    pool_provider: Option<PoolProviderFactory>,
    shard_provider: Option<ShardProviderFactory>,
    shard_router: Option<Arc<dyn crate::sharding::ShardRouter>>,
    directory_shard_router: bool,
    hook_queue_migration_mode: RepositoryCommitHookQueueMigrationMode,
) -> Result<DatabaseBootstrap, String> {
    // #1628: declare replication BEFORE any pool exists, so every pooled SQLite
    // connection is created with `wal_autocheckpoint = 0` and the replicator is
    // the only component that ever checkpoints. The flag latches on and is never
    // cleared — see `set_sqlite_replication_active`.
    if config.replication.as_ref().is_some_and(|r| r.enabled) {
        crate::db::set_sqlite_replication_active();
    }
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
    // Fail-closed statement-timeout guard, enforced only once a control pool has
    // actually been established — the provider, built-in or custom, returned
    // `Some(..)`. The built-in `create_topology`/`create_shard_topology` factories
    // validate `database.statement_timeout` internally, but a custom
    // `with_pool_provider` provider can build its own SQLite pool without them:
    // the default `DatabasePoolProvider::create_topology` only delegates to
    // `create_pool`, and both factories are overridable, so a custom provider
    // could otherwise discard the timeout and break the fail-closed guarantee.
    // Under the `sqlite` feature `RuntimeBackend` is always SQLite, so an
    // established pool plus a nonzero timeout is exactly the fail-closed
    // condition. A provider returning `Ok(None)` opts into the supported
    // no-database mode — no pool or statement needs a timeout — so it must still
    // boot, matching the built-in path. Gating on `topology.is_some()` preserves
    // that opt-out. The check is idempotent with the built-in factories' own, and
    // `resolve_shard_set` applies the same Some-gated guard for shards.
    #[cfg(feature = "sqlite")]
    if topology.is_some() {
        crate::db::reject_sqlite_statement_timeout(config.database.statement_timeout)
            .map_err(|e| format!("Failed to create database pool: {e}"))?;
    }

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

    // Skip migrations when the provider opted out of a database by returning
    // `Ok(None)`, even with `database.url` configured. Custom providers signal
    // "this app runs without a DB" that way, and migrating against the URL anyway
    // would defeat the opt-out.
    //
    // A provider may also resolve its primary URL at runtime (managed Postgres)
    // and carry it on the topology. Prefer that URL, so migrations target the pool
    // actually built rather than a stale or absent configured one.
    let provider_migration_url = topology
        .as_ref()
        .and_then(|t| t.migration_url())
        .map(str::to_owned);

    // SQLite sharding guard (#1614, PR3). The SQLite startup-migration path applies
    // registered migrations to a `sqlite://` control target — `run_startup_migrations`
    // routes them through `crate::migrate::auto_migrate_sqlite` — so registered
    // migrations no longer fail fast here. Sharding is what remains unsupported:
    // the directory/shard-map control migrations and per-shard fan-out are
    // Postgres-specific. So a sqlite control target with sharding enabled fails
    // fast, as does any `sqlite:` shard `primary_url`, because the shard loop
    // routes each shard through the Postgres-only harness. Empty when unsharded or
    // when the shard loop will not run. See `sqlite_sharding_unsupported_guard`.
    #[cfg(feature = "sqlite")]
    let sqlite_guard_shard_urls: Vec<&str> = if shards.is_some() {
        config
            .database
            .shards
            .iter()
            .map(|shard| shard.primary_url.as_str())
            .collect()
    } else {
        Vec::new()
    };
    #[cfg(feature = "sqlite")]
    #[allow(clippy::question_mark)] // managed-pg child must be stopped before returning
    if let Err(e) = sqlite_sharding_unsupported_guard(
        if topology.is_some() {
            provider_migration_url
                .as_deref()
                .or_else(|| config.database.effective_primary_url())
        } else {
            None
        },
        directory_migration_required
            || shard_map_migration_required
            || config.database.has_shards(),
        &sqlite_guard_shard_urls,
    ) {
        #[cfg(feature = "managed-pg")]
        crate::managed_pg::emergency_stop_async().await;
        return Err(e);
    }

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
/// Apply pending migrations for one target in the `AUTUMN_MIGRATE=1` one-shot,
/// returning the number applied — or exiting non-zero on failure.
///
/// Uses the same locked applier the startup path uses
/// ([`run_pending_locked`](crate::migrate::run_pending_locked)). Failure messages
/// are REDACTED to a value-free reason plus the target label (`control` /
/// `shard:<name>`): the underlying [`MigrationError`](crate::migrate::MigrationError)
/// can wrap a driver string that embeds the connection URL, and the deploy path
/// must never print a DB URL or secret.
#[cfg(feature = "db")]
fn apply_pending_or_exit(
    database_url: &str,
    migrations: impl diesel::migration::MigrationSource<diesel::pg::Pg> + Send,
    target: &str,
) -> usize {
    match crate::migrate::run_pending_locked(database_url, migrations, None) {
        Ok(result) => result.applied.len(),
        Err(error) => {
            let reason = match error {
                crate::migrate::MigrationError::Connection(_) => {
                    "could not connect to the database"
                }
                crate::migrate::MigrationError::Migration(_) => "a migration failed to apply",
                _ => "migration error",
            };
            eprintln!("autumn migrate: {reason} (target {target})");
            // `process::exit` skips `on_shutdown`/`Drop`; stop any managed
            // Postgres child first so a failure doesn't orphan the data dir/port.
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop();
            std::process::exit(1);
        }
    }
}

/// Apply pending migrations for one `SQLite` target in the `AUTUMN_MIGRATE=1`
/// one-shot, returning the number applied — or exiting non-zero on failure
/// (issue #1614, PR3).
///
/// The `SQLite` counterpart to [`apply_pending_or_exit`]: it uses the unlocked
/// [`run_pending_sqlite`](crate::migrate::run_pending_sqlite) harness (`SQLite`
/// is single-writer, so there is no advisory lock and no cross-process race to
/// serialize) and REDACTS failure messages to a value-free reason plus the
/// target label, exactly like the Postgres path, so a driver string embedding
/// the database path is never printed.
#[cfg(feature = "sqlite")]
fn apply_pending_sqlite_or_exit(
    database_url: &str,
    migrations: impl diesel::migration::MigrationSource<diesel::sqlite::Sqlite> + Send,
    target: &str,
) -> usize {
    // Reject ANY in-memory target (private OR shared-cache) with registered
    // migrations up front, BEFORE `run_pending_sqlite` (whose `Migration` error
    // would be redacted to a value-free reason below, hiding the guidance). The
    // migrated schema never survives to the runtime pool — a private `:memory:`
    // connection is its own empty database, and a shared in-memory database is
    // destroyed when its last connection closes (issue #1614 follow-up).
    if let Some(err) = crate::migrate::reject_in_memory_migrations(database_url, &migrations) {
        eprintln!("autumn migrate: {err} (target {target})");
        std::process::exit(1);
    }
    match crate::migrate::run_pending_sqlite(database_url, migrations) {
        Ok(result) => result.applied.len(),
        Err(error) => {
            let reason = match error {
                crate::migrate::MigrationError::Connection(_) => {
                    "could not connect to the database"
                }
                crate::migrate::MigrationError::Migration(_) => "a migration failed to apply",
                _ => "migration error",
            };
            eprintln!("autumn migrate: {reason} (target {target})");
            std::process::exit(1);
        }
    }
}

/// Guard the startup-migration path against a **sharded** `SQLite` deployment.
///
/// PR3 (#1614) wired a working `SQLite` startup-migration path: registered
/// migrations now apply to a `sqlite://` control target through diesel's
/// `MigrationHarness` on a `SqliteConnection`, with no advisory lock (see
/// [`crate::migrate::run_pending_sqlite`] / [`crate::migrate::auto_migrate_sqlite`],
/// routed from [`run_startup_migrations`]). So registered migrations are no
/// longer rejected here — a `sqlite://` control target with `.migrations(...)`
/// boots and applies its schema.
///
/// What remains unsupported is **sharding on `SQLite`**: the shard-directory and
/// shard-map control tables and the per-shard fan-out are Postgres/sharding
/// primitives (advisory locks, Postgres DDL), and a single-node `SQLite`
/// deployment has no shards. Two situations are therefore rejected here with an
/// actionable message rather than attempting to create Postgres-shaped sharding
/// tables on `SQLite`:
///
///   * a `SQLite` **control** target for which the sharding control migrations
///     are required or shards are configured (`control_sharding_required` —
///     `directory_migration_required || shard_map_migration_required ||
///     config.database.has_shards()`); and
///   * any `SQLite` **shard** `primary_url`: the shard loop in
///     [`run_startup_migrations`] migrates every shard through the Postgres-only
///     harness, so a `sqlite:` shard is sharding-on-`SQLite` regardless of the
///     control backend. `shard_urls` is empty when the app is unsharded or the
///     shard loop won't run, so the Postgres path is unaffected.
///
/// Uses the same [`crate::config::DatabaseBackend::detect`] predicate as
/// `db::build_pool`, so the gate and the pool routing agree on what "is a
/// `SQLite` URL" means.
#[cfg(feature = "sqlite")]
fn sqlite_sharding_unsupported_guard(
    control_url: Option<&str>,
    control_sharding_required: bool,
    shard_urls: &[&str],
) -> Result<(), String> {
    fn is_sqlite(url: &str) -> bool {
        crate::config::DatabaseBackend::detect(url) == Some(crate::config::DatabaseBackend::Sqlite)
    }
    if control_url.is_some_and(is_sqlite) && control_sharding_required {
        return Err(
            "SQLite deployments do not support sharding. The configured sqlite:// control target \
             has sharding enabled (shards and/or the directory/shard-map control migrations), \
             which is a Postgres-only capability \u{2014} remove the shard configuration to run \
             on SQLite, or use a Postgres control database. Tracking: #1614."
                .to_owned(),
        );
    }
    if shard_urls.iter().copied().any(is_sqlite) {
        return Err(
            "SQLite deployments do not support sharding. A configured shard targets a SQLite \
             database, and per-shard migration/fan-out is a Postgres-only capability \u{2014} \
             remove the SQLite shard configuration to run on SQLite, or use Postgres shard \
             targets. Tracking: #1614."
                .to_owned(),
        );
    }
    Ok(())
}

#[cfg(all(test, feature = "sqlite"))]
mod sqlite_sharding_unsupported_guard_tests {
    use super::sqlite_sharding_unsupported_guard;

    #[test]
    fn sqlite_control_target_with_sharding_fails_fast() {
        // PR3: a sqlite:// control URL with sharding enabled must fail fast with
        // an actionable, sharding-named boot error — sharding is Postgres-only.
        for url in [
            "sqlite:///var/lib/app.db",
            "sqlite://./relative.db",
            "sqlite::memory:",
        ] {
            let err = sqlite_sharding_unsupported_guard(Some(url), true, &[])
                .expect_err("sqlite control target + sharding must be rejected");
            assert!(
                err.contains("do not support sharding"),
                "message must name the sharding situation clearly: {err}"
            );
            assert!(
                err.contains("#1614"),
                "message must point at the tracking issue: {err}"
            );
        }
    }

    #[test]
    fn sqlite_control_target_without_sharding_boots() {
        // PR3 behavior: a sqlite target with registered (non-sharding)
        // migrations now boots — they are applied by `auto_migrate_sqlite`.
        for url in [
            "sqlite:///var/lib/app.db",
            "sqlite://./relative.db",
            "sqlite::memory:",
        ] {
            assert!(
                sqlite_sharding_unsupported_guard(Some(url), false, &[]).is_ok(),
                "sqlite target without sharding must boot (migrations now applied): {url}"
            );
        }
    }

    #[test]
    fn postgres_target_is_unchanged() {
        // The default Postgres path is untouched, sharding required or not.
        for url in [
            "postgres://u@h/db",
            "postgresql://user:pass@db:5432/app",
            "host=db user=app sslmode=require",
        ] {
            assert!(
                sqlite_sharding_unsupported_guard(Some(url), true, &[]).is_ok(),
                "postgres target must never be gated: {url}"
            );
            assert!(
                sqlite_sharding_unsupported_guard(Some(url), false, &[]).is_ok(),
                "postgres target must never be gated: {url}"
            );
        }
    }

    #[test]
    fn absent_control_url_boots() {
        // No control URL (no-DB / opt-out provider): nothing to gate.
        assert!(sqlite_sharding_unsupported_guard(None, true, &[]).is_ok());
        assert!(sqlite_sharding_unsupported_guard(None, false, &[]).is_ok());
    }

    #[test]
    fn sqlite_shard_target_fails_fast() {
        // A `sqlite:` shard `primary_url` is sharding-on-SQLite regardless of the
        // control backend and regardless of migrations — the shard loop routes it
        // through the Postgres-only harness. It must be rejected with the
        // actionable sharding error.
        for shards in [
            &["sqlite:///var/lib/shard0.db"][..],
            &["sqlite:///var/lib/shard0.db", "postgres://u@h/shard1"][..],
        ] {
            let err = sqlite_sharding_unsupported_guard(None, false, shards)
                .expect_err("a sqlite shard target must be rejected");
            assert!(
                err.contains("do not support sharding") && err.contains("shard"),
                "message must name the SQLite shard situation clearly: {err}"
            );
            assert!(
                err.contains("#1614"),
                "message must point at the tracking issue: {err}"
            );
        }
    }

    #[test]
    fn postgres_shard_targets_are_unchanged() {
        // All-Postgres shards are never gated, sharding required or not.
        assert!(
            sqlite_sharding_unsupported_guard(
                Some("postgres://u@h/control"),
                true,
                &["postgres://u@h/shard0", "postgres://u@h/shard1"],
            )
            .is_ok(),
            "all-postgres shard targets must never be gated"
        );
    }

    #[test]
    fn migrate_only_mode_reuses_the_boot_guard_for_sqlite_targets() {
        // `run_migrate_only_mode` (the `AUTUMN_MIGRATE=1` one-shot) applies the
        // SAME guard as normal boot BEFORE its migration loop, so the boot and
        // migrate-only paths cannot drift. A sqlite control/shard target with
        // sharding still fails fast; a plain sqlite control target now proceeds
        // (its migrations are applied by the sqlite apply path).

        // A sqlite CONTROL migrate target with sharding → actionable error.
        let err = sqlite_sharding_unsupported_guard(Some("sqlite:///var/lib/app.db"), true, &[])
            .expect_err("sqlite migrate control target with sharding must be rejected");
        assert!(
            err.contains("do not support sharding") && err.contains("#1614"),
            "migrate-only sqlite control error must be the actionable sharding message: {err}"
        );

        // A sqlite SHARD migrate target → actionable error.
        let shard_err = sqlite_sharding_unsupported_guard(
            Some("postgres://u@h/control"),
            true,
            &["sqlite:///var/lib/shard0.db"],
        )
        .expect_err("sqlite migrate shard target must be rejected");
        assert!(
            shard_err.contains("do not support sharding") && shard_err.contains("shard"),
            "migrate-only sqlite shard error must be the actionable sharding message: {shard_err}"
        );

        // An all-Postgres migrate configuration (control + shards) is never gated.
        assert!(
            sqlite_sharding_unsupported_guard(
                Some("postgres://u@h/control"),
                true,
                &["postgres://u@h/shard0"],
            )
            .is_ok(),
            "an all-postgres migrate configuration must proceed unchanged"
        );

        // A plain sqlite migrate control target (no sharding) proceeds — its
        // migrations are applied by the sqlite apply path.
        assert!(
            sqlite_sharding_unsupported_guard(Some("sqlite:///var/lib/app.db"), false, &[]).is_ok(),
            "sqlite control target without sharding must never be gated"
        );
    }
}

/// Combine the app's registered migration sets with the two standalone
/// shard-directory / shard-map control migrations — applied straight from
/// their own `const`s rather than through `migrations` — into one input for
/// [`crate::migrate::compute_migration_disambiguation`]. Without this, a
/// plugin claiming one of those fixed, framework-owned versions under a
/// different name would skip past collision detection entirely, since
/// neither standalone set is otherwise part of the registered `migrations`
/// this function is given.
///
/// The two standalone sets are included only when `shards_configured` is
/// true. An unsharded app (including every `sqlite` app, which rejects
/// sharding outright) never applies either set, so including them
/// unconditionally would treat their fixed versions as live collision
/// participants for an app that will never actually record them — risking
/// an already-applied, unrelated migration being reassigned a new
/// substitute the moment this framework version is adopted, purely because
/// of a coincidental version match against a migration that was never a
/// real collision for that app. `shards_configured` (`database.has_shards()`)
/// is a conservative superset of the precise runtime conditions
/// `directory_migration_required`/`shard_map_migration_required` gate the
/// actual apply on (both imply shards are configured; the converse doesn't
/// hold for e.g. an explicit custom shard router) — deliberately so: BOTH
/// call sites (`run_startup_migrations` and the `autumn migrate` CLI path,
/// which cannot cheaply reconstruct the precise runtime flags) must use the
/// IDENTICAL condition, or the two paths could reach different
/// disambiguation decisions for the same migration depending on which one
/// happens to run first — the exact hazard this whole mechanism exists to
/// avoid. The residual narrow case (a sharded app using an explicit custom
/// router) may see a harmless, unnecessary disambiguation entry for a shard
/// version it will never apply; that is safe, just imprecise.
#[cfg(feature = "db")]
fn migration_sets_for_disambiguation<'a>(
    migrations: &'a [(&'static str, crate::migrate::EmbeddedMigrations)],
    shards_configured: bool,
) -> Vec<(&'a str, &'a crate::migrate::EmbeddedMigrations)> {
    let owned = migrations.iter().map(|(name, set)| (*name, set));
    if shards_configured {
        owned
            .chain([
                (
                    "shard-directory",
                    &crate::sharding::SHARD_DIRECTORY_MIGRATIONS,
                ),
                ("shard-map", &crate::sharding::SHARD_MAP_MIGRATIONS),
            ])
            .collect()
    } else {
        owned.collect()
    }
}

#[cfg(feature = "db")]
#[allow(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    clippy::too_many_lines
)]
async fn run_startup_migrations(
    config: &AutumnConfig,
    control_configured: bool,
    shards_configured: bool,
    provider_migration_url: Option<String>,
    migrations: Vec<(&'static str, crate::migrate::EmbeddedMigrations)>,
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
    let auto_migrate = config.database.auto_migrate;
    let auto_in_prod = config.database.auto_migrate_in_production;
    // Computed once, on the FINAL registered set (after `setup_database`'s own
    // fold added any shard-required sets), so a version collision between ANY
    // two registered sources is resolved automatically rather than causing
    // one migration to be silently skipped — see
    // `compute_migration_disambiguation` and `migration_sets_for_disambiguation`.
    let disambiguation_sets =
        migration_sets_for_disambiguation(&migrations, config.database.has_shards());
    let disambiguated = crate::migrate::compute_migration_disambiguation(&disambiguation_sets);
    let migration_result = tokio::task::spawn_blocking(move || {
        // SQLite single-writer startup-migration path (#1614, PR3): apply the
        // registered migrations to a `sqlite://` control target with no advisory
        // lock. Sharding — directory, shard-map, per-shard fan-out — is
        // Postgres-only and is rejected upstream in `setup_database` by
        // `sqlite_sharding_unsupported_guard`, so nothing shard- or
        // directory-related runs here, and the framework migrations below are
        // skipped for a SQLite control target.
        #[cfg(feature = "sqlite")]
        if let Some(url) = control_url.as_deref()
            && crate::config::DatabaseBackend::detect(url)
                == Some(crate::config::DatabaseBackend::Sqlite)
        {
            for (_, mig) in &migrations {
                crate::migrate::auto_migrate_sqlite(
                    url,
                    profile.as_deref(),
                    auto_migrate,
                    auto_in_prod,
                    crate::migrate::DisambiguatedMigrations::new(mig, &disambiguated),
                    "control",
                );
            }
            return;
        }

        if let Some(url) = control_url {
            for (_, mig) in &migrations {
                crate::migrate::auto_migrate(
                    &url,
                    profile.as_deref(),
                    auto_migrate,
                    auto_in_prod,
                    crate::migrate::DisambiguatedMigrations::new(mig, &disambiguated),
                    "control",
                );
            }
            // The shard directory table lives on the control plane only, so it
            // is applied here and never to the per-shard targets below.
            if directory_migration_required {
                crate::migrate::auto_migrate(
                    &url,
                    profile.as_deref(),
                    auto_migrate,
                    auto_in_prod,
                    crate::migrate::DisambiguatedMigrations::new(
                        &crate::sharding::SHARD_DIRECTORY_MIGRATIONS,
                        &disambiguated,
                    ),
                    "control",
                );
            }
            // The shard-map guard table also lives on the control plane only. It
            // follows the same resolved, profile-agnostic auto-migrate decision as
            // the app migrations above (#1903): default-on in dev, opt-in in prod
            // via `auto_migrate`/`auto_migrate_in_production`, over the same
            // advisory-locked apply path. Under a report-only decision the missing
            // table is reported rather than force-applied, so a DB-free or offline
            // startup never fails fatally on an unreachable control target.
            if shard_map_migration_required {
                crate::migrate::auto_migrate(
                    &url,
                    profile.as_deref(),
                    auto_migrate,
                    auto_in_prod,
                    crate::migrate::DisambiguatedMigrations::new(
                        &crate::sharding::SHARD_MAP_MIGRATIONS,
                        &disambiguated,
                    ),
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
            for (_, mig) in migrations
                .iter()
                .filter(|(_, mig)| !migration_set_is_control_framework(mig))
            {
                crate::migrate::auto_migrate(
                    url,
                    profile.as_deref(),
                    auto_migrate,
                    auto_in_prod,
                    crate::migrate::DisambiguatedMigrations::new(mig, &disambiguated),
                    target,
                );
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
        let assignments: Vec<_> = computed.to_vec();
        conn.transaction::<(), diesel::result::Error, _>(async move |conn| {
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
// Sharding (the shard-map guard, auto-split, and control-DB shard map) is a
// Postgres-only feature: `run_shard_map_guard` drives Postgres `sql_query`
// against a `Pool<AsyncPgConnection>` control pool. Under the `sqlite` feature
// the runtime topology's pool is a SQLite pool that cannot feed it, and SQLite
// deployments are single-node/unsharded, so the guard is a no-op.
#[cfg(all(feature = "db", feature = "sqlite"))]
#[allow(clippy::unused_async)]
async fn enforce_shard_map_guard(
    config: &AutumnConfig,
    topology: Option<&crate::db::DatabaseTopology>,
    runtime_boot: bool,
) -> Result<(), String> {
    let _ = (config, topology, runtime_boot);
    Ok(())
}

#[cfg(all(feature = "db", not(feature = "sqlite")))]
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
    mut migrations: Vec<(&'static str, crate::migrate::EmbeddedMigrations)>,
    hook_queue_required: bool,
    version_history_required: bool,
    mode: RepositoryCommitHookQueueMigrationMode,
) -> Vec<(&'static str, crate::migrate::EmbeddedMigrations)> {
    if hook_queue_required
        && mode == RepositoryCommitHookQueueMigrationMode::Runtime
        && !shard_applied_sets_include(&migrations, REPOSITORY_COMMIT_HOOK_QUEUE_MIGRATION)
    {
        migrations.push((
            "repository-commit-hooks",
            crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS,
        ));
    }
    if version_history_required
        && mode == RepositoryCommitHookQueueMigrationMode::Runtime
        && !shard_applied_sets_include(&migrations, VERSION_HISTORY_MIGRATION)
    {
        migrations.push((
            "version-history",
            crate::version_history::VERSION_HISTORY_MIGRATIONS,
        ));
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
    migrations: &[(&'static str, crate::migrate::EmbeddedMigrations)],
    migration_name: &str,
) -> bool {
    use diesel::migration::{Migration, MigrationSource as _};
    use diesel::pg::Pg;

    migrations
        .iter()
        .filter(|(_, set)| !migration_set_is_control_framework(set))
        .any(|(_, source)| {
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
mod agent_authority_route_summary_tests {

    use super::*;

    fn route_with(path: &'static str, mcp_tool: bool) -> Route {
        let mut api_doc = crate::openapi::ApiDoc {
            method: "GET",
            path,
            operation_id: "list_items",
            mcp_tool,
            ..crate::openapi::ApiDoc::default()
        };
        // The exposure rule is JSON-out gated, so a route with no response
        // schema is never a tool no matter what it is tagged with. Give it one.
        api_doc.response = Some(crate::openapi::SchemaEntry {
            name: "Item",
            kind: crate::openapi::SchemaKind::Ref,
            identity: None,
        });
        Route {
            method: http::Method::GET,
            path,
            handler: axum::routing::any(|| async { "" }),
            name: "list_items",
            api_doc,
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        }
    }

    #[test]
    fn a_scoped_route_records_the_path_an_agent_actually_calls() {
        // A scoped group's children carry only the child path -- the prefix is
        // applied at mount time. Recording `/items` for a tool served at
        // `/api/v1/items` would make the manifest wrong about where the agent
        // surface is, and a scope rename would produce no drift at all.
        let route = route_with("/items", true);
        let summary = agent_authority_route_summary(&route, Some("/api/v1"), false);
        assert_eq!(summary.path, "/api/v1/items");
        assert_eq!(summary.method, "GET");
        assert!(summary.mcp_tool);

        // A top-level route has no prefix and is unchanged.
        let top = agent_authority_route_summary(&route, None, false);
        assert_eq!(top.path, "/items");
    }

    #[test]
    fn a_scoped_root_route_joins_the_way_the_openapi_collector_does() {
        // Delegated to `join_nested_path` rather than string concatenation, so
        // the manifest and the spec cannot disagree about trailing slashes.
        let root = route_with("/", true);
        let summary = agent_authority_route_summary(&root, Some("/api"), false);
        assert_eq!(
            summary.path,
            crate::router::join_nested_path("/api", "/"),
            "the manifest must join paths exactly as the spec does"
        );
    }

    #[test]
    fn the_operation_id_names_the_tool_and_exposure_says_why() {
        use crate::agent_authority::manifest::McpExposedBy;

        let tagged = agent_authority_route_summary(&route_with("/items", true), None, false);
        assert_eq!(tagged.operation_id, "list_items");
        assert_eq!(tagged.exposed_by, Some(McpExposedBy::Attribute));

        // Untagged and no hatch: not a tool at all.
        let plain = agent_authority_route_summary(&route_with("/items", false), None, false);
        assert!(!plain.mcp_tool);
        assert_eq!(plain.exposed_by, None);

        // Untagged, but the whole-API hatch sweeps up a read-only verb.
        let hatched = agent_authority_route_summary(&route_with("/items", false), None, true);
        assert!(hatched.mcp_tool);
        assert_eq!(hatched.exposed_by, Some(McpExposedBy::Hatch));
    }
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
            seo: crate::seo::SeoRouteDefaults::EMPTY,
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
            _conn: &'a mut crate::db::RuntimeConnection,
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

/// Publish the builder's story gallery (if any) as the [`StoryRegistry`]
/// (`crate::stories::StoryRegistry`) `AppState` extension read by the
/// `/_stories` handlers. Shared by the `run()` and build/SSG
/// state-construction paths so the two stay in lockstep.
#[cfg(feature = "maud")]
fn install_story_registry(state: &AppState, story_gallery: Option<crate::stories::StoryGallery>) {
    if let Some(gallery) = story_gallery {
        state.insert_extension(gallery.into_registry());
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
        #[cfg(all(feature = "db", feature = "reporting"))]
        db_capture_gap: database_topology
            .and_then(|topology| topology.capture_gap().map(std::sync::Arc::from)),
        profile: config.profile.as_deref().map(Arc::from),
        role: config.role,
        started_at: crate::time::monotonic_now(),
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
        auth_session_key: Arc::from(config.auth.session_key.as_str()),
        shared_cache: None,
        clock: std::sync::Arc::new(crate::time::SystemClock),
        entropy: std::sync::Arc::new(crate::entropy::OsEntropy),
        app_id: AppState::next_app_id(),
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

/// Serve in-place upgrades for the lifetime of the process (issue #1674).
///
/// Each `SIGUSR2` attempts one handover. A failed attempt is logged and the
/// current build carries on serving — including its live state, which is
/// unfrozen again — so a later signal (with a fixed binary) can retry.
#[cfg(unix)]
async fn watch_for_in_place_upgrade(
    config: &crate::config::UpgradeConfig,
    signal: Option<tokio::signal::unix::Signal>,
    socket: Option<crate::upgrade::HandoffSocket>,
    state: AppState,
    cutover: tokio_util::sync::CancellationToken,
) {
    // Registered at the top of `run()` so the signal is never fatal; `None`
    // means the handler could not be installed at all, which was reported then.
    let Some(mut signal) = signal else {
        return;
    };
    // Clamped: a zero-second budget would abandon every successor before it
    // could possibly have finished booting.
    let ready_timeout = std::time::Duration::from_secs(config.ready_timeout_secs.max(1));

    while signal.recv().await.is_some() {
        if !config.enabled {
            tracing::warn!(
                "SIGUSR2 received but in-place upgrade is disabled \
                 ([server.upgrade] enabled = false); ignoring"
            );
            continue;
        }
        // Documented as incompatible, and refused rather than attempted: the
        // successor would start a second postmaster over the same data
        // directory, and this process's drain would stop the cluster under it.
        #[cfg(feature = "managed-pg")]
        if crate::managed_pg::is_supervising() {
            let error = crate::upgrade::UpgradeError::Unsupported(
                "it supervises a managed Postgres cluster, which cannot be handed over with \
                 the socket — the successor would start a second postmaster over the same \
                 data directory",
            );
            tracing::error!(error = %error, "in-place upgrade refused");
            continue;
        }
        let Some(socket) = socket.as_ref() else {
            let error = crate::upgrade::UpgradeError::UnsupportedListener(
                "the server is not bound to a plain TCP listener",
            );
            tracing::error!(error = %error, "in-place upgrade refused");
            continue;
        };
        tracing::info!("SIGUSR2 received, starting an in-place upgrade");
        let plan = crate::upgrade::UpgradePlan {
            socket,
            registry: state.extension::<crate::upgrade::LiveStateRegistry>(),
            ready_timeout,
            clock: state.clock_arc(),
            entropy: state.entropy_arc(),
        };
        match crate::upgrade::upgrade_in_place(plan).await {
            Ok(handover) => {
                tracing::info!(
                    successor_pid = handover.successor_pid,
                    generation = handover.generation,
                    elapsed_ms = u64::try_from(handover.elapsed.as_millis()).unwrap_or(u64::MAX),
                    "in-place upgrade complete: the successor is serving on this socket, \
                     draining this build"
                );
                cutover.cancel();
                return;
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "in-place upgrade abandoned; this build is still serving and its live \
                     state is writable again"
                );
            }
        }
    }
}

enum DrainCause {
    /// `SIGTERM`/Ctrl-C, or a canary rollback flag: the process is going away
    /// and its address goes with it.
    Signal,
    /// An in-place upgrade (#1674): a successor is already accepting on this
    /// process's listening socket, so the address stays up throughout.
    UpgradeCutover,
}

/// Environment variable naming a file whose appearance drains this app.
///
/// Windows has no `SIGTERM`, so a parent process that supervises an Autumn app
/// (today: `autumn dev`) could only stop it with `TerminateProcess` — which
/// skips `on_shutdown` hooks entirely, orphaning a managed Postgres child on
/// every hot reload (issue #1616). Setting this variable gives such a parent a
/// portable way to request the *same* graceful drain a signal triggers: create
/// the file, and the app runs its normal shutdown sequence.
///
/// Opt-in: unset (or empty), nothing watches anything. The file's contents are
/// never read — its existence is the whole signal — so a parent can create it
/// with a plain zero-byte `File::create`. It must be a regular file; a directory
/// at that path is ignored.
///
/// **Honored on non-Unix targets only.** On Unix `SIGTERM` already does this
/// job, and arming a file-triggered drain there would put a production
/// deployment one operator-configured path away from being drainable by anything
/// that can create a file. The variable is accepted and ignored on Unix.
pub const SHUTDOWN_SIGNAL_FILE_ENV: &str = "AUTUMN_SHUTDOWN_SIGNAL_FILE";

/// Resolve the cooperative-shutdown file from a raw environment value.
///
/// Split out from the env read so it is testable without mutating process-global
/// state. A blank value is treated as unset: an exported-but-empty variable must
/// not resolve to the current directory, where an unrelated file would drain the
/// app.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix drain arm, compiled and tested on every platform"
    )
)]
fn external_shutdown_path_from(value: Option<&std::ffi::OsStr>) -> Option<std::path::PathBuf> {
    let trimmed = value?.to_string_lossy().trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    // Trim before building the path, not just before testing for emptiness: a
    // value with stray whitespace would otherwise resolve to a sibling path the
    // parent never writes, and the failure would be silent — every reload
    // stalling the full budget and hard-killing, which is the bug this exists
    // to fix.
    Some(std::path::PathBuf::from(trimmed))
}

/// Resolve when the cooperative-shutdown file at `path` exists.
///
/// Never resolves when `path` is `None` (the feature is not configured), so an
/// app that does not opt in is unaffected. Resolves immediately when the file is
/// already present at boot: a supervising parent removes a stale file before
/// spawning, so a file that survives into a fresh process means a stop was
/// requested and unhandled — draining is the safe direction.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix drain arm, compiled and tested on every platform"
    )
)]
async fn external_shutdown_signal(path: Option<std::path::PathBuf>) {
    let Some(path) = path else {
        std::future::pending::<()>().await;
        return;
    };
    let interval = std::time::Duration::from_millis(100);
    loop {
        // `is_file`, not "exists": `metadata` succeeds on a directory, so a
        // variable pointed at one would drain the app on every boot — bind,
        // drain, exit 0 — which a supervisor reads as a healthy restart loop
        // rather than the misconfiguration it is.
        if tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_file()) {
            return;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Wait for a shutdown signal (Ctrl+C, SIGTERM on Unix, or a canary rollback
/// flag file written by a controller).
///
/// Returns the reason the drain began. Also resolves on an in-place
/// upgrade cutover (#1674), whose drain skips the readiness flip and the
/// prestop grace because the address never goes away.
///
/// Returns when any signal is received. Axum's `with_graceful_shutdown`
/// then stops accepting new connections and drains in-flight requests.
///
/// The canary rollback arm lets a progressive-delivery controller drain and
/// retire a bad canary replica without sending `SIGTERM` by hand: it writes
/// [`crate::canary::CANARY_ROLLBACK_FLAG_FILE`] and Autumn runs the identical
/// graceful-shutdown sequence (ready → 503, prestop grace, drain, clean exit).
async fn shutdown_signal(upgrade_cutover: tokio_util::sync::CancellationToken) -> DrainCause {
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

    // A supervising parent with no `SIGTERM` to send — `autumn dev` on Windows —
    // can request this exact drain by creating a file (#1616).
    //
    // Deliberately `cfg`-gated to the platforms that need it, rather than armed
    // everywhere and left inert by an unset variable. Autumn deploys to Linux,
    // where this would be a production drain trigger reachable by anything that
    // can create a configured path, with no authentication and no benefit, since
    // `SIGTERM` already exists. The helpers stay compiled and unit-tested
    // everywhere.
    #[cfg(not(unix))]
    let external_stop = async {
        let path =
            external_shutdown_path_from(std::env::var_os(SHUTDOWN_SIGNAL_FILE_ENV).as_deref());
        external_shutdown_signal(path.clone()).await;
        tracing::warn!(
            path = ?path,
            "Cooperative shutdown requested via {SHUTDOWN_SIGNAL_FILE_ENV}, starting graceful shutdown"
        );
    };

    #[cfg(unix)]
    let external_stop = std::future::pending::<()>();

    let upgrade = async {
        upgrade_cutover.cancelled().await;
        tracing::info!("Successor is serving after an in-place upgrade, draining this build");
    };

    tokio::select! {
        () = ctrl_c => DrainCause::Signal,
        () = terminate => DrainCause::Signal,
        () = canary_rollback => DrainCause::Signal,
        () = external_stop => DrainCause::Signal,
        () = upgrade => DrainCause::UpgradeCutover,
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
    /// Regression (#1620): DNS-01 must not silence the multi-replica warning.
    ///
    /// DNS-01 retires the HTTP-01 token-map hazard — the CA never connects to
    /// this host — but it does not distribute certificates. A Postgres-backed
    /// (i.e. multi-replica) fleet still has every non-leader replica serving the
    /// self-signed placeholder off its own empty on-disk store, so it must still
    /// warn, naming the certificate store rather than tokens.
    #[cfg(feature = "acme")]
    #[test]
    fn acme_fleet_warning_covers_the_certificate_store_under_dns01() {
        use crate::config::SchedulerBackend;

        // Single-replica: silent under either challenge type.
        assert!(super::acme_fleet_warning(SchedulerBackend::InProcess, false).is_none());
        assert!(super::acme_fleet_warning(SchedulerBackend::InProcess, true).is_none());

        // Multi-replica HTTP-01: both hazards named.
        let http01 = super::acme_fleet_warning(SchedulerBackend::Postgres, false)
            .expect("a distributed backend under HTTP-01 must warn");
        assert!(
            http01.contains("token"),
            "HTTP-01 warning must name the token store: {http01}"
        );

        // Multi-replica DNS-01: still warns, about the certificate store.
        let dns01 = super::acme_fleet_warning(SchedulerBackend::Postgres, true).expect(
            "a distributed backend under DNS-01 must still warn: DNS-01 proves domain \
                     control but does not share the certificate store",
        );
        assert!(
            dns01.contains("certificate store"),
            "DNS-01 warning must name the certificate store as the hazard: {dns01}"
        );
        assert!(
            dns01.contains("cache_dir"),
            "DNS-01 warning must name the config key an operator would change: {dns01}"
        );
        // …and must not repeat the HTTP-01 diagnosis, which does not apply.
        assert!(
            !dns01.contains("token") && !dns01.contains("404"),
            "DNS-01 warning must not blame the HTTP-01 token map: {dns01}"
        );
    }

    /// A clock near the end of representable time must not kill the scheduler.
    ///
    /// `format_next_task_run_after` runs at the top of every fixed-delay loop.
    /// While `now` came from real time its sum with an ordinary delay could not
    /// overflow, but an injected clock is unbounded, and chrono's `Add` panics
    /// rather than wrapping — so a `FixedClock` near `DateTime::MAX_UTC` would
    /// take the task down before its first run, over a string for a log line.
    #[test]
    fn next_task_run_saturates_instead_of_panicking_at_the_end_of_time() {
        let max = chrono::DateTime::<chrono::Utc>::MAX_UTC;

        // An ordinary delay that would carry `now` past the representable end.
        let rendered = super::format_next_task_run_after(max, std::time::Duration::from_secs(60));
        assert_eq!(
            rendered,
            max.to_rfc3339(),
            "a fixed delay past the end of time must clamp, not panic"
        );

        // A delay too large for `TimeDelta` at all renders as the far future,
        // not as `now` — the latter would read as "this task runs immediately".
        let absurd = super::format_next_task_run_after(
            chrono::Utc::now(),
            std::time::Duration::from_secs(u64::MAX),
        );
        assert_eq!(absurd, max.to_rfc3339());

        // The ordinary case is unchanged.
        let epoch = chrono::DateTime::from_timestamp(0, 0).unwrap();
        assert_eq!(
            super::format_next_task_run_after(epoch, std::time::Duration::from_secs(60)),
            (epoch + chrono::TimeDelta::seconds(60)).to_rfc3339()
        );
    }

    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    // ── omitted-router accounting for `autumn routes audit` ──────────────────

    // #1974 item 7: a plugin declares a top-level config section from its
    // `build()` via `config_section`, so `server.strict_config` treats that root
    // as known-and-opaque. Prove the declaration lands on the builder and that
    // the registry is fail-closed (an undeclared root is not registered).
    #[test]
    fn plugin_declares_config_section_via_build() {
        struct DummyMediaPlugin;
        impl crate::plugin::Plugin for DummyMediaPlugin {
            fn build(self, app: AppBuilder) -> AppBuilder {
                app.config_section("media")
            }
        }

        let builder = app().plugin(DummyMediaPlugin);
        assert!(
            builder.has_config_section("media"),
            "a plugin's build() must declare its [media] config section"
        );
        assert!(
            !builder.has_config_section("definitely_not_a_root"),
            "only explicitly-declared roots are registered — the seam is fail-closed"
        );
    }

    /// Compute the omitted-router count for a builder using the same inputs
    /// `run_dump_routes_mode` feeds `omitted_router_count`: the merge count, the
    /// nest prefixes, and the declared routes that prove nest coverage.
    fn omitted_for(builder: &AppBuilder) -> usize {
        omitted_router_count(
            builder.merge_routers.len(),
            builder
                .nest_routers
                .iter()
                .map(|(prefix, _)| prefix.as_str()),
            &builder.declared_routes,
        )
    }

    /// Regression (#1604): the DOCUMENTED plugin pattern —
    /// `app.nest(prefix, router).declare_plugin_routes(routes)`, which the
    /// first-party `AdminPlugin` uses — declares route metadata whose paths fall
    /// under the nest prefix. That coverage makes the nested raw router
    /// enumerable, so it must NOT be counted among the opaque, omitted routers
    /// that hard-fail the audit gate. Before the fix, prefix coverage was
    /// ignored and the mere presence of the nested raw router pushed
    /// `hidden > 0`, false-failing the audit even though every admin route was
    /// declared and classified.
    #[test]
    fn documented_nest_then_declare_is_not_counted_as_omitted() {
        let raw =
            axum::Router::<AppState>::new().route("/ping", axum::routing::get(|| async { "pong" }));
        let declared = vec![crate::route_listing::RouteInfo {
            method: "GET".to_owned(),
            path: "/admin/ping".to_owned(),
            handler: "admin::ping".to_owned(),
            ..Default::default()
        }];

        // The plain documented pattern: nest the raw router, then declare its
        // covering route metadata. No dedicated `nest_declared` bookkeeping.
        let builder = app().nest("/admin", raw).declare_plugin_routes(declared);

        // The raw router is still mounted (serving path is unchanged) …
        assert_eq!(builder.nest_routers.len(), 1);
        // … and its declared metadata was folded into `declared_routes`.
        assert_eq!(builder.declared_routes.len(), 1);

        // ⇒ zero omitted routers: the declared route `/admin/ping` falls under
        // the `/admin` nest prefix, so the mount is covered and the audit gate
        // must NOT fire.
        assert_eq!(
            omitted_for(&builder),
            0,
            "a nest whose endpoints are declared is enumerable and must not count as omitted",
        );
    }

    /// The soundness guarantee must survive: a raw router mounted via bare
    /// `nest()` or `merge()` without covering declarations is unenumerable and
    /// must still count as omitted so `autumn routes audit` fails closed.
    #[test]
    fn undeclared_nest_and_merge_still_count_as_omitted() {
        let raw_nest =
            axum::Router::<AppState>::new().route("/x", axum::routing::get(|| async { "x" }));
        let raw_merge =
            axum::Router::<AppState>::new().route("/y", axum::routing::get(|| async { "y" }));

        let builder = app().nest("/v2", raw_nest).merge(raw_merge);

        assert_eq!(builder.nest_routers.len(), 1);
        assert_eq!(builder.merge_routers.len(), 1);
        // Nothing was declared, so nothing covers the nest.
        assert!(builder.declared_routes.is_empty());

        assert_eq!(
            omitted_for(&builder),
            2,
            "an undeclared nest and a merge are both opaque and must be reported",
        );
    }

    /// An undeclared `merge()` is rootless — it cannot be prefix-matched — so it
    /// stays omitted even when unrelated declared routes exist. Guards against a
    /// declaration for one mount silently covering an unrelated raw `merge()`.
    #[test]
    fn declared_routes_do_not_cover_a_rootless_merge() {
        let raw_merge =
            axum::Router::<AppState>::new().route("/y", axum::routing::get(|| async { "y" }));

        let builder =
            app()
                .merge(raw_merge)
                .declare_plugin_routes(vec![crate::route_listing::RouteInfo {
                    method: "GET".to_owned(),
                    path: "/admin/ok".to_owned(),
                    handler: "admin::ok".to_owned(),
                    ..Default::default()
                }]);

        assert_eq!(
            omitted_for(&builder),
            1,
            "a merge has no prefix to match declarations against and must always count",
        );
    }

    /// A declared nest alongside an *undeclared* nest: only the undeclared one
    /// is omitted. Prefix-matching must cover the `/admin` mount (a declared
    /// route falls under it) without spilling onto the unrelated `/raw` mount
    /// (no declared route falls under it).
    #[test]
    fn mixed_declared_and_undeclared_nests_count_only_the_undeclared() {
        let declared_raw =
            axum::Router::<AppState>::new().route("/ok", axum::routing::get(|| async { "ok" }));
        let undeclared_raw = axum::Router::<AppState>::new()
            .route("/opaque", axum::routing::get(|| async { "opaque" }));

        let builder = app()
            .nest("/admin", declared_raw)
            .declare_plugin_routes(vec![crate::route_listing::RouteInfo {
                method: "GET".to_owned(),
                path: "/admin/ok".to_owned(),
                handler: "admin::ok".to_owned(),
                ..Default::default()
            }])
            .nest("/raw", undeclared_raw);

        assert_eq!(builder.nest_routers.len(), 2);
        assert_eq!(builder.declared_routes.len(), 1);
        assert_eq!(
            omitted_for(&builder),
            1,
            "only the bare nest() is omitted; the declared mount is covered",
        );
    }

    /// A declared route whose path merely *shares a leading substring* with a
    /// nest prefix (`/administrators` vs `/admin`) must NOT cover the nest:
    /// prefix-matching honours path-segment boundaries, so this bare nest still
    /// counts as omitted and the audit fails closed.
    #[test]
    fn prefix_match_respects_path_segment_boundaries() {
        let raw = axum::Router::<AppState>::new().route("/x", axum::routing::get(|| async { "x" }));

        let builder = app().nest("/admin", raw).declare_plugin_routes(vec![
            crate::route_listing::RouteInfo {
                method: "GET".to_owned(),
                path: "/administrators".to_owned(),
                handler: "other::index".to_owned(),
                ..Default::default()
            },
        ]);

        assert_eq!(
            omitted_for(&builder),
            1,
            "`/administrators` is not under the `/admin` nest prefix; the nest stays omitted",
        );
    }

    #[test]
    fn is_dump_jobs_mode_only_true_for_exactly_one() {
        // `autumn jobs manifest` sets AUTUMN_DUMP_JOBS=1 to select the manifest
        // dump path in `run()`. Any other value (or an unset var) must fall
        // through to the normal boot path.
        temp_env::with_var("AUTUMN_DUMP_JOBS", Some("1"), || {
            assert!(is_dump_jobs_mode(), "`1` must select the jobs-dump path");
        });
        temp_env::with_var("AUTUMN_DUMP_JOBS", Some("0"), || {
            assert!(!is_dump_jobs_mode(), "`0` must not select the dump path");
        });
        temp_env::with_var("AUTUMN_DUMP_JOBS", Some("true"), || {
            assert!(
                !is_dump_jobs_mode(),
                "only the literal `1` enables the mode"
            );
        });
        temp_env::with_var("AUTUMN_DUMP_JOBS", None::<&str>, || {
            assert!(!is_dump_jobs_mode(), "unset must not select the dump path");
        });
    }

    #[test]
    fn is_retention_dry_run_mode_only_true_for_exactly_one() {
        // `autumn retention --dry-run` sets AUTUMN_RETENTION_DRY_RUN=1 to
        // select the dry-run path in `run()`. Any other value (or an unset
        // var) must fall through to the normal boot path (issue #1342).
        temp_env::with_var("AUTUMN_RETENTION_DRY_RUN", Some("1"), || {
            assert!(
                is_retention_dry_run_mode(),
                "`1` must select the retention dry-run path"
            );
        });
        temp_env::with_var("AUTUMN_RETENTION_DRY_RUN", Some("0"), || {
            assert!(
                !is_retention_dry_run_mode(),
                "`0` must not select the dry-run path"
            );
        });
        temp_env::with_var("AUTUMN_RETENTION_DRY_RUN", Some("true"), || {
            assert!(
                !is_retention_dry_run_mode(),
                "only the literal `1` enables the mode"
            );
        });
        temp_env::with_var("AUTUMN_RETENTION_DRY_RUN", None::<&str>, || {
            assert!(
                !is_retention_dry_run_mode(),
                "unset must not select the dry-run path"
            );
        });
    }

    #[cfg(feature = "db")]
    #[test]
    fn retention_dry_run_model_filter_from_env_trims_and_ignores_blank() {
        temp_env::with_var("AUTUMN_RETENTION_MODEL", Some("  Widget  "), || {
            assert_eq!(
                retention_dry_run_model_filter_from_env().as_deref(),
                Some("Widget")
            );
        });
        temp_env::with_var("AUTUMN_RETENTION_MODEL", Some(""), || {
            assert_eq!(retention_dry_run_model_filter_from_env(), None);
        });
        temp_env::with_var("AUTUMN_RETENTION_MODEL", None::<&str>, || {
            assert_eq!(retention_dry_run_model_filter_from_env(), None);
        });
    }

    #[cfg(feature = "db")]
    #[test]
    fn merge_and_validate_task_names_rejects_collision_with_hand_declared_task() {
        // `resolve_retention_descriptors` validates collisions only among
        // retention-generated task names; it cannot see hand-declared `tasks![...]`
        // entries, which real boot merges in through `AppBuilder::build`'s
        // `tasks.extend(...)` and `validate_unique_scheduled_task_names`. Without
        // this check a dry run could report success for a policy whose generated
        // name collides with a hand-declared task, while real boot panics on that
        // same collision.
        fn dry_run_stub(
            _state: AppState,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = crate::AutumnResult<crate::retention::RetentionSweepReport>,
                    > + Send,
            >,
        > {
            Box::pin(async {
                Ok(crate::retention::RetentionSweepReport {
                    model: "Widget".to_string(),
                    table: "widgets".to_string(),
                    rows_swept: 0,
                    duration_ms: 0,
                    dry_run: true,
                })
            })
        }
        fn task_info_stub() -> crate::task::TaskInfo {
            crate::task::TaskInfo {
                name: "retention-sweep-widgets".to_string(),
                schedule: crate::task::Schedule::FixedDelay(std::time::Duration::from_secs(3600)),
                coordination: crate::task::TaskCoordination::Fleet,
                handler: |_state| Box::pin(async { Ok(()) }),
            }
        }
        let descriptor = crate::retention::RetentionSweepDescriptor {
            model_name: "Widget",
            table_name: "widgets",
            task_info: task_info_stub,
            dry_run: dry_run_stub,
        };
        let hand_declared_collision = crate::task::TaskInfo {
            name: "retention-sweep-widgets".to_string(),
            schedule: crate::task::Schedule::FixedDelay(std::time::Duration::from_secs(60)),
            coordination: crate::task::TaskCoordination::PerReplica,
            handler: |_state| Box::pin(async { Ok(()) }),
        };

        let error = merge_and_validate_task_names(&[&descriptor], vec![hand_declared_collision])
            .expect_err("a hand-declared task colliding with a generated task name must error");

        assert!(
            error.contains("retention-sweep-widgets"),
            "the error must name the colliding task: {error}"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn merge_and_validate_task_names_accepts_distinct_names() {
        fn dry_run_stub(
            _state: AppState,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = crate::AutumnResult<crate::retention::RetentionSweepReport>,
                    > + Send,
            >,
        > {
            Box::pin(async {
                Ok(crate::retention::RetentionSweepReport {
                    model: "Widget".to_string(),
                    table: "widgets".to_string(),
                    rows_swept: 0,
                    duration_ms: 0,
                    dry_run: true,
                })
            })
        }
        fn task_info_stub() -> crate::task::TaskInfo {
            crate::task::TaskInfo {
                name: "retention-sweep-widgets".to_string(),
                schedule: crate::task::Schedule::FixedDelay(std::time::Duration::from_secs(3600)),
                coordination: crate::task::TaskCoordination::Fleet,
                handler: |_state| Box::pin(async { Ok(()) }),
            }
        }
        let descriptor = crate::retention::RetentionSweepDescriptor {
            model_name: "Widget",
            table_name: "widgets",
            task_info: task_info_stub,
            dry_run: dry_run_stub,
        };
        let unrelated_task = crate::task::TaskInfo {
            name: "nightly-report".to_string(),
            schedule: crate::task::Schedule::FixedDelay(std::time::Duration::from_secs(60)),
            coordination: crate::task::TaskCoordination::PerReplica,
            handler: |_state| Box::pin(async { Ok(()) }),
        };

        assert!(merge_and_validate_task_names(&[&descriptor], vec![unrelated_task]).is_ok());
    }

    #[test]
    fn retention_dry_run_one_shot_dispatches_before_server_start() {
        // Mirrors `migrate_only_one_shot_applies_and_exits_without_serving`:
        // AUTUMN_RETENTION_DRY_RUN=1 must be handled BEFORE the `let Self {`
        // destructure that begins the serving path, so a dry-run never binds
        // a port (issue #1342).
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let run_start = source.find("pub async fn run(self)").expect("run() exists");
        let run_end = source
            .find("async fn run_build_mode(self)")
            .expect("build mode follows run()");
        let run_body = &source[run_start..run_end];

        let dispatch = run_body
            .find("if is_retention_dry_run_mode() {")
            .expect("run() dispatches the retention dry-run one-shot");
        let server_start = run_body
            .find("let Self {")
            .expect("run() destructures self to start the server");
        assert!(
            dispatch < server_start,
            "AUTUMN_RETENTION_DRY_RUN must be handled before the server-start path"
        );
        let dry_run_branch = &run_body[dispatch..server_start];
        assert!(
            dry_run_branch.contains("self.run_retention_dry_run_mode().await;")
                && dry_run_branch.contains("return;"),
            "the retention dry-run one-shot must run then return before server start"
        );
    }

    #[test]
    fn dump_jobs_manifest_includes_synthesized_durable_listener_default_queue() {
        // Regression (#1802, Codex P2): an app that registers a durable listener
        // and configures `[jobs.queues]` WITHOUT `default` still drains `default`
        // at runtime — `finalize_event_bus` synthesizes a `default`-queue
        // `JobInfo` for each durable listener before the runtime starts. The
        // `AUTUMN_DUMP_JOBS=1` manifest must reflect that same set through the
        // shared `synthesize_durable_listener_jobs` seam, or a topology-aware
        // `autumn doctor` would accept a fleet where no tier drains those jobs.
        fn listener_handler(
            _state: AppState,
            _payload: serde_json::Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'static>,
        > {
            Box::pin(async move { Ok(()) })
        }
        let durable = crate::events::ListenerInfo {
            event_name: "UserSignedUp",
            listener_name: "app::send_welcome_email".to_string(),
            mode: crate::events::DispatchMode::Durable,
            job_name: Some("__event_listener::send_welcome_email".to_string()),
            max_attempts: 4,
            initial_backoff_ms: 250,
            handler: listener_handler,
        };
        let cfg = crate::config::JobQueuesConfig::strict_list(["critical"]);

        // The dump path holds no builder-registered jobs, only the durable
        // listener; the manifest must still surface `default` (where that
        // listener's synthesized job runs) alongside the configured `critical`.
        let manifest = dump_jobs_manifest(&cfg, Vec::new(), vec![durable]);
        assert_eq!(manifest, "queues = [\"critical\", \"default\"]\n");
    }

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
            health_detailed: true,
            ..AppState::test_default()
        };
        crate::router::build_router(routes, &config, state)
    }

    // ── Cooperative external shutdown (#1616) ──────────────────────────────
    //
    // On Windows there is no SIGTERM: `autumn dev` could only stop the app with
    // `TerminateProcess`, which skips `on_shutdown` hooks — so a managed
    // Postgres child was orphaned on every hot reload. These cover the runtime
    // half of the fix: an opt-in, env-named flag file that drains the app
    // through the *same* graceful path a signal takes.

    #[test]
    fn external_shutdown_path_is_none_when_the_env_var_is_absent() {
        assert_eq!(external_shutdown_path_from(None), None);
    }

    #[test]
    fn external_shutdown_path_ignores_an_empty_env_value() {
        // An empty value is what an unset-but-exported variable looks like. It
        // must not resolve to the current directory, where any stray file would
        // drain the app.
        assert_eq!(
            external_shutdown_path_from(Some(std::ffi::OsStr::new(""))),
            None
        );
        assert_eq!(
            external_shutdown_path_from(Some(std::ffi::OsStr::new("   "))),
            None
        );
    }

    #[test]
    fn external_shutdown_path_trims_surrounding_whitespace() {
        // A value that only *looks* blank is rejected above; one with real
        // content and stray whitespace must resolve to the path the parent
        // actually wrote, not to a sibling with a leading space that will
        // never match. Getting this wrong is silent: on Windows every reload
        // would stall the full budget and hard-kill, reintroducing the
        // orphaned-cluster bug this seam exists to fix.
        assert_eq!(
            external_shutdown_path_from(Some(std::ffi::OsStr::new("  /tmp/autumn-stop  "))),
            Some(std::path::PathBuf::from("/tmp/autumn-stop"))
        );
    }

    #[tokio::test]
    async fn external_shutdown_signal_ignores_a_directory_at_the_path() {
        // `metadata()` succeeds on a directory. If existence alone were the
        // test, pointing the variable at a directory would drain the app on
        // every boot — bind, drain, exit 0 — which under a supervisor is a
        // restart loop that reports success on every iteration.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("not-a-signal");
        std::fs::create_dir(&path).unwrap();

        let resolved = tokio::time::timeout(
            std::time::Duration::from_millis(400),
            external_shutdown_signal(Some(path)),
        )
        .await;
        assert!(resolved.is_err(), "a directory must not count as a signal");
    }

    #[test]
    fn external_shutdown_path_uses_the_env_value_verbatim() {
        assert_eq!(
            external_shutdown_path_from(Some(std::ffi::OsStr::new("/tmp/autumn-stop"))),
            Some(std::path::PathBuf::from("/tmp/autumn-stop"))
        );
    }

    #[tokio::test]
    async fn external_shutdown_signal_never_resolves_without_a_path() {
        // Not configured must mean "never drains" — not "drains immediately",
        // which would take down every app that does not opt in.
        let pending = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            external_shutdown_signal(None),
        )
        .await;
        assert!(
            pending.is_err(),
            "an unconfigured signal must never resolve"
        );
    }

    #[tokio::test]
    async fn external_shutdown_signal_resolves_when_the_file_appears() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("dev-shutdown.signal");

        let writer_path = path.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            std::fs::write(&writer_path, b"stop").unwrap();
        });

        let signalled = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            external_shutdown_signal(Some(path)),
        )
        .await;
        assert!(signalled.is_ok(), "writing the file must drain the app");
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn external_shutdown_signal_resolves_immediately_when_present_at_boot() {
        // `autumn dev` removes a stale file before spawning, but a crashed
        // parent can leave one behind; resolving immediately is the safe
        // direction — the app exits gracefully rather than ignoring a stop.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("dev-shutdown.signal");
        std::fs::write(&path, b"stop").unwrap();

        let signalled = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            external_shutdown_signal(Some(path)),
        )
        .await;
        assert!(
            signalled.is_ok(),
            "a file present at boot must drain the app"
        );
    }

    #[test]
    fn external_shutdown_env_var_is_the_name_the_cli_writes() {
        // `autumn-cli` mirrors this constant (it cannot depend on autumn-web's
        // private items); a rename on either side breaks the dev loop silently,
        // so pin the wire name here.
        assert_eq!(SHUTDOWN_SIGNAL_FILE_ENV, "AUTUMN_SHUTDOWN_SIGNAL_FILE");
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

    #[test]
    fn build_state_exposes_resolved_process_role() {
        use crate::config::ProcessRole;

        // Default config resolves to the combined role: existing single-process
        // apps see no behavior change and get both HTTP + workers.
        let mut config = AutumnConfig::default();
        let state = build_state(
            &config,
            #[cfg(feature = "db")]
            None,
            #[cfg(feature = "db")]
            None,
            #[cfg(feature = "ws")]
            None,
        );
        assert_eq!(state.role(), ProcessRole::Combined);
        assert!(state.role().serves_http());
        assert!(state.role().runs_workers());

        // A worker-role config flows through the exact same resolution the
        // framework uses to gate the job runtime, reachable via `state.role()`.
        config.role = ProcessRole::Worker;
        let state = build_state(
            &config,
            #[cfg(feature = "db")]
            None,
            #[cfg(feature = "db")]
            None,
            #[cfg(feature = "ws")]
            None,
        );
        assert_eq!(state.role(), ProcessRole::Worker);
        assert!(state.role().runs_workers());
        assert!(!state.role().serves_http());
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
                    diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
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

    // Finding 2 (Codex P2), corrected: the fail-closed `statement_timeout` guard
    // must fire once a custom provider has actually established a pool. A custom
    // provider can build its own SQLite pool without the built-in
    // `create_topology`/`create_pool` factories — the default `create_topology`
    // only delegates to `create_pool`, and both are overridable — so
    // `setup_database` enforces the guard at dispatch. It must run only for an
    // established pool (`Some(..)`), not before the provider returns: a provider
    // returning `Ok(None)` opts into the supported no-database mode and must still
    // boot even with a nonzero `statement_timeout`, matching the built-in path.
    // CI's sqlite job runs the named integration targets, not `--lib`, and
    // `setup_database` and the shared guard are crate-private, so this boundary is
    // reachable only from a unit test.
    //
    // Case (a): a custom provider that establishes a real in-memory SQLite pool
    // (`Ok(Some(..))`) under a nonzero `statement_timeout` must fail closed with
    // the actionable error, keeping the original F2 bypass closed.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn custom_pool_provider_with_established_sqlite_pool_fails_closed_on_statement_timeout() {
        // Builds a real in-memory SQLite pool WITHOUT routing the timeout through
        // the built-in factory — the exact F2 bypass a custom provider could use.
        struct RealSqlitePoolProvider;

        impl crate::db::DatabasePoolProvider for RealSqlitePoolProvider {
            async fn create_pool(
                &self,
                config: &crate::config::DatabaseConfig,
            ) -> Result<
                Option<
                    diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
                >,
                crate::db::PoolError,
            > {
                // Clear `statement_timeout` locally so the built-in `create_pool`
                // guard does not fire inside the provider — this provider hands
                // back a live SQLite pool that silently dropped the timeout, which
                // is exactly the fail-closed condition the dispatch guard closes.
                let mut relaxed = config.clone();
                relaxed.statement_timeout = None;
                crate::db::create_pool(&relaxed)
            }
        }

        let mut config = AutumnConfig::default();
        config.database.primary_url = Some("sqlite::memory:".to_owned());
        config.database.statement_timeout = Some(std::time::Duration::from_secs(30));
        let AppBuilder {
            pool_provider_factory,
            shard_provider_factory,
            ..
        } = app().with_pool_provider(RealSqlitePoolProvider);

        let Err(err) = setup_database(
            &config,
            Vec::new(),
            pool_provider_factory,
            shard_provider_factory,
            None,
            false,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        )
        .await
        else {
            panic!(
                "sqlite + statement_timeout must fail closed once the provider establishes a pool"
            );
        };
        assert!(
            err.contains("database.statement_timeout") && err.contains("SQLite"),
            "dispatch guard error must name the config key and SQLite, got: {err}"
        );
    }

    // Case (b) — the exact regression Codex flagged: a custom provider that opts
    // into no-database mode (`Ok(None)`) must STILL boot even with a nonzero
    // `database.statement_timeout`, because no pool/statement exists to bound. The
    // Some-gated guard must not reject this the way the pre-dispatch guard did.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn custom_pool_provider_no_database_mode_boots_with_statement_timeout() {
        struct NoDatabaseProvider;

        impl crate::db::DatabasePoolProvider for NoDatabaseProvider {
            async fn create_pool(
                &self,
                _config: &crate::config::DatabaseConfig,
            ) -> Result<
                Option<
                    diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
                >,
                crate::db::PoolError,
            > {
                // Explicit no-database opt-out.
                Ok(None)
            }
        }

        let mut config = AutumnConfig::default();
        // A URL may even be configured; the provider's `Ok(None)` still wins.
        config.database.primary_url = Some("sqlite::memory:".to_owned());
        config.database.statement_timeout = Some(std::time::Duration::from_secs(30));
        let AppBuilder {
            pool_provider_factory,
            shard_provider_factory,
            ..
        } = app().with_pool_provider(NoDatabaseProvider);

        let bootstrap = setup_database(
            &config,
            Vec::new(),
            pool_provider_factory,
            shard_provider_factory,
            None,
            false,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        )
        .await
        .expect("no-database provider must boot even with a nonzero statement_timeout");
        assert!(
            bootstrap.topology.is_none(),
            "no-database mode must yield no control topology"
        );
        assert!(
            bootstrap.shards.is_none(),
            "no-database mode must yield no shard set"
        );
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
                    diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
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
        let server_init = "initialize_job_runtime(
            jobs,
            &state,
            &server_shutdown,";
        let server_worker = "start_repository_commit_hook_worker(\n                pool,\n                server_shutdown.child_token(),\n            );";
        let task_init = "initialize_job_runtime(jobs, &state, &task_shutdown, &config.jobs, true)";
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

    #[cfg(feature = "db")]
    #[test]
    fn repository_commit_hook_workers_are_gated_on_worker_role() {
        // Draining durable after-commit hook rows is background execution, so the
        // primary-pool and shard commit-hook worker starts on the normal server
        // path must both be guarded by `role.runs_workers()` — a web-role replica
        // must not claim/execute hook rows (that work belongs to the worker tier).
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let primary_gate =
            "if role.runs_workers()\n            && let Some(pool) = state.pool().cloned()";
        let shard_gate = "if role.runs_workers()\n            && let Some(shards) = state.shards()";
        assert!(
            source.contains(primary_gate),
            "primary-pool commit-hook worker must be gated on role.runs_workers()"
        );
        assert!(
            source.contains(shard_gate),
            "shard commit-hook workers must be gated on role.runs_workers()"
        );
    }

    /// The replication loop's final flush must be *triggered* after the drain.
    ///
    /// Cancelling the token only *wakes* the replication thread; the tick that
    /// ships the last committed frames runs immediately after it. So waiting
    /// for the thread is not enough on its own — if the token fires at phase 5,
    /// with `server_shutdown`, that final tick happens while requests are still
    /// draining, and a request that commits during the drain is never
    /// replicated even though the wait below succeeds and logs that replication
    /// flushed. The token must therefore be independent of `server_shutdown`
    /// and cancelled only once the drain has finished.
    ///
    /// The wait must also not be a blocking join: a blocked join cannot be
    /// cancelled, so a stuck upload would hold the process open past the
    /// shutdown budget that is supposed to bound it.
    ///
    /// Source-order test in the house style, because the ordering is a property
    /// of this function and nothing smaller: an app-level boot test would need a
    /// `SQLite` runtime pool, which only exists in the `sqlite` lane.
    #[cfg(feature = "db")]
    #[test]
    fn the_replication_final_flush_is_waited_for_after_the_drain() {
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let server_start = source
            .find("pub async fn run(self)")
            .expect("normal server path should exist");
        // Bounded at the next path so the search cannot match this test's own
        // source, which necessarily quotes the strings it is looking for.
        let build_mode_start = source
            .find("async fn run_build_mode(self)")
            .expect("static build path should follow server path");
        let server_source = &source[server_start..build_mode_start];

        let spawn = server_source
            .find("Ok(_handle) => replication_done = Some(waiter),")
            .expect("the replication thread's completion signal must be kept, not dropped");
        let drain = server_source
            .find("let server_result = server_task.await")
            .expect("the normal server path should await the drain");
        let wait = server_source
            .find("if let Some(waiter) = replication_done.take()")
            .expect("shutdown must wait for the replication thread");
        let cancel = server_source
            .find("replication_shutdown.cancel();")
            .expect("shutdown must cancel the replication token explicitly");

        assert!(
            spawn < drain && drain < wait && drain < cancel,
            "the final flush must be triggered AND awaited AFTER the request drain \
             (spawn={spawn}, drain={drain}, wait={wait}, cancel={cancel})"
        );
        // The token that releases the final tick must not fire with the
        // listener: that is the whole bug this ordering exists to prevent.
        assert!(
            server_source
                .contains("let replication_shutdown = tokio_util::sync::CancellationToken::new();"),
            "the replication token must be independent of `server_shutdown`, not a child of it"
        );
        assert!(
            !server_source[..wait].contains("let replication_shutdown = server_shutdown"),
            "the replication token must not be derived from `server_shutdown`"
        );
        // A blocking join cannot be abandoned when the budget runs out.
        assert!(
            !server_source[wait..].contains("spawn_blocking"),
            "the bounded wait must not block on a join it cannot cancel"
        );
    }

    /// The cluster must outlive the in-flight request drain.
    ///
    /// `server_shutdown` fires at phase 5, when the listener stops accepting —
    /// requests keep draining after it for up to `shutdown_timeout_secs`, and a
    /// request served in that window can still increment a cluster counter. If
    /// the cluster's loops were children of that token they would already have
    /// departed, so the increment would land in a document with nothing left to
    /// replicate it and die with the process: a write accepted and silently
    /// thrown away. Source-order test in the house style, because the ordering
    /// is a property of this function and nothing smaller.
    #[test]
    fn cluster_departs_after_the_request_drain() {
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let server_start = source
            .find("pub async fn run(self)")
            .expect("normal server path should exist");
        // Bounded at the next path so the search cannot match this test's own
        // source, which necessarily quotes the strings it is looking for.
        let build_mode_start = source
            .find("async fn run_build_mode(self)")
            .expect("static build path should follow server path");
        let server_source = &source[server_start..build_mode_start];

        let install = server_source
            .find("crate::cluster::install_from_config(&state, &config.cluster, &cluster_shutdown)")
            .expect("the cluster must be installed on its own token, not on server_shutdown");
        let drain = server_source
            .find("let server_result = server_task.await")
            .expect("the normal server path should await the drain");
        let depart = server_source
            .find("cluster_shutdown.cancel();")
            .expect("the cluster token should be cancelled explicitly");

        assert!(
            install < drain && drain < depart,
            "the cluster must be installed before the drain and cancelled only \
             after it completes, so an increment accepted while requests drain \
             still has a push loop to replicate it"
        );
        assert!(
            !server_source.contains("&config.cluster, &server_shutdown"),
            "the cluster must not ride server_shutdown: that token fires when \
             the listener closes, with the request drain still ahead of it"
        );
    }

    /// …and it must depart *inside* the shutdown budget, not after it.
    ///
    /// Drain, departure and `on_shutdown` hooks share one
    /// `shutdown_timeout_secs`: a supervisor that times the process out at that
    /// deadline `SIGKILL`s whatever is still running. An unconditional
    /// `LEAVE_BUDGET` sleep between the drain and the hooks would make the
    /// worst case `shutdown_timeout_secs + LEAVE_BUDGET`, so the wait is
    /// clamped to the budget the drain left over.
    #[test]
    fn the_cluster_departure_wait_fits_inside_the_shutdown_budget() {
        use std::time::Duration;

        let budget = Duration::from_secs(30);

        // A fast drain: the departure gets its whole budget…
        let quick = Duration::from_secs(1);
        assert_eq!(
            cluster_departure_wait(budget, quick),
            crate::cluster::LEAVE_BUDGET,
            "a drain that finished early must leave room for the full departure"
        );

        // …but never more than the node itself will spend on it.
        assert!(
            cluster_departure_wait(budget, Duration::ZERO) <= crate::cluster::LEAVE_BUDGET,
            "waiting longer than LEAVE_BUDGET would idle past the node's own bound"
        );

        // A drain that ran to the deadline gets no departure at all: the peer
        // converges on the suspicion timeout instead.
        assert_eq!(
            cluster_departure_wait(budget, budget),
            Duration::ZERO,
            "a drain that consumed the budget must not buy extra shutdown time"
        );
        assert_eq!(
            cluster_departure_wait(budget, budget.saturating_add(Duration::from_secs(5))),
            Duration::ZERO,
            "an overrun drain must saturate, not wrap into a fresh wait"
        );

        // The property the finding is about: drain, departure and hooks are one
        // budget. `hook_budget` is `budget - elapsed` measured *after* the
        // departure wait, so whatever the drain leaves over has to cover both
        // of the phases that follow it — never budget + LEAVE_BUDGET.
        for elapsed_ms in [0_u64, 250, 29_800, 29_999, 30_000, 45_000] {
            let drained = Duration::from_millis(elapsed_ms);
            let departure = cluster_departure_wait(budget, drained);
            let hooks = budget.saturating_sub(drained.saturating_add(departure));
            let left_by_the_drain = budget.saturating_sub(drained);
            assert!(
                departure.saturating_add(hooks) <= left_by_the_drain,
                "after a {drained:?} drain only {left_by_the_drain:?} of the \
                 {budget:?} budget is left, but the departure ({departure:?}) \
                 and hooks ({hooks:?}) would spend more"
            );
        }
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
        let server_init = "initialize_job_runtime(
            jobs,
            &state,
            &server_shutdown,";
        let task_init = "initialize_job_runtime(jobs, &state, &task_shutdown, &config.jobs, true)";
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

    #[test]
    fn graph_installed_before_every_router_build() {
        // The architecture graph (#1747) is published by whichever path is
        // about to build a router, and `/actuator/graph` answers from what was
        // published. Codex round 1 found the install wired into the
        // static-build and capsule-replay paths but NOT into `run()` — so every
        // unit test passed while a normally running app answered 503 forever.
        // A structural assertion, because the serving path ends in `serve()`
        // and cannot be driven from a unit test.
        let whole = include_str!("app.rs").replace("\r\n", "\n");
        // Only the non-test source: this test names both strings itself.
        let source = whole
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map_or(whole.as_str(), |(before, _)| before)
            .to_owned();
        let builds: Vec<usize> = source
            .match_indices("crate::router::try_build_router")
            .map(|(i, _)| i)
            // Skip doc-comment and test references; only real call sites are
            // followed by an open paren on the same expression.
            .filter(|i| source[*i..].starts_with("crate::router::try_build_router"))
            .filter(|i| {
                let tail = &source[*i..*i + 80];
                tail.contains("_inner(") || tail.contains("_with_static_inner(")
            })
            .collect();
        assert!(
            builds.len() >= 3,
            "expected the serving, static-build and replay router builds: {}",
            builds.len()
        );
        let installs: Vec<usize> = source
            .match_indices("crate::graph::install(")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            installs.len(),
            3,
            "every path that builds an application router must publish the graph \
             it is about to serve"
        );
        // Each install precedes a router build with no other install between.
        for install in &installs {
            assert!(
                builds.iter().any(|build| build > install),
                "an install with no router build after it is dead code"
            );
        }
        for build in &builds {
            // A probe-only router (worker role) serves the actuator too, but it
            // is built inside the same `run()` block the serving install covers.
            assert!(
                installs.iter().any(|install| install < build),
                "a router built with no graph published before it answers 503 at \
                 /actuator/graph"
            );
        }
    }

    #[test]
    fn migrate_only_one_shot_applies_and_exits_without_serving() {
        // The runtime effect (applying against Postgres, exiting without binding a
        // port) needs a DB + a subprocess harness because `run()` ends in
        // `process::exit`; that live apply is exercised by the shared
        // `run_pending_locked` engine's own DB-backed tests. Here we lock the
        // *dispatch decision* and the *reuse/exit contract* structurally: with
        // AUTUMN_MIGRATE=1 the migrate-and-exit path is chosen and the
        // server-start path is NOT taken.
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let run_start = source.find("pub async fn run(self)").expect("run() exists");
        let run_end = source
            .find("async fn run_build_mode(self)")
            .expect("build mode follows run()");
        let run_body = &source[run_start..run_end];

        // The AUTUMN_MIGRATE=1 dispatch is an early one-shot: it sits BEFORE the
        // server-start machinery (the `let Self {` destructure that begins the
        // serving path) and returns, so a migrate run never binds a port.
        let dispatch = run_body
            .find("if is_migrate_only_mode() {")
            .expect("run() dispatches the migrate one-shot");
        let server_start = run_body
            .find("let Self {")
            .expect("run() destructures self to start the server");
        assert!(
            dispatch < server_start,
            "AUTUMN_MIGRATE must be handled before the server-start path"
        );
        let migrate_branch = &run_body[dispatch..server_start];
        assert!(
            migrate_branch.contains("self.run_migrate_only_mode().await;")
                && migrate_branch.contains("return;"),
            "the migrate one-shot must run then return before server start"
        );

        // The handler applies per target and exits — never starting the server.
        let handler_start = source
            .find("async fn run_migrate_only_mode(self)")
            .expect("migrate handler exists");
        let handler_end = source
            .find("async fn run_one_off_task_mode(self, requested_name: String)")
            .expect("one-off task handler follows the migrate handler");
        let handler = &source[handler_start..handler_end];
        assert!(
            handler.contains("apply_pending_or_exit"),
            "the migrate handler applies pending migrations per target"
        );
        assert!(
            handler.contains("std::process::exit(0)"),
            "the migrate handler exits after applying"
        );

        // Issue #1614, PR3: the migrate one-shot must apply the SAME SQLite
        // sharding guard as normal boot BEFORE its migration loop, so a sharded
        // `sqlite:` target exits with the actionable sharding error instead of a
        // generic `PgConnection` failure — and the two paths cannot drift because
        // both call `sqlite_sharding_unsupported_guard`.
        let guard_call = handler
            .find("sqlite_sharding_unsupported_guard(")
            .expect("migrate handler applies the SQLite sharding guard");
        let first_apply = handler
            .find("apply_pending_or_exit")
            .expect("migrate handler applies per target");
        assert!(
            guard_call < first_apply,
            "the SQLite guard must run BEFORE the migration loop / apply_pending_or_exit"
        );
        assert!(
            !handler.contains("initialize_job_runtime")
                && !handler.contains("try_build_router_inner"),
            "the migrate one-shot must not start the server"
        );

        // The per-target applier reuses `run_pending_locked` (the exact engine
        // `auto_migrate` drives — no duplicated migration logic) and exits
        // non-zero on failure so a bad migration aborts before cutover (AC-3).
        let helper_start = source
            .find("fn apply_pending_or_exit(")
            .expect("apply_pending_or_exit exists");
        let helper = &source[helper_start..helper_start + 1200];
        assert!(
            helper.contains("crate::migrate::run_pending_locked("),
            "must reuse the shared locked applier, not duplicate migration logic"
        );
        assert!(
            helper.contains("std::process::exit(1)"),
            "a failed migration must exit non-zero (abort before cutover)"
        );
    }

    /// The replay handler's source, between its signature and the next
    /// top-level item after the `AppBuilder` impl block.
    #[cfg(feature = "reporting")]
    fn replay_mode_source(source: &str) -> &str {
        let start = source
            .find("async fn run_replay_mode(self, capsule_path: String)")
            .expect("replay handler exists");
        let end = source
            .find("pub(crate) fn is_static_build_mode()")
            .expect("the mode predicates follow the AppBuilder impl block");
        source
            .get(start..end)
            .expect("the replay handler precedes the mode predicates")
    }

    #[cfg(feature = "reporting")]
    #[test]
    fn replay_one_shot_runs_before_server_start() {
        // `AUTUMN_REPLAY_CAPSULE=<path>` (set by `autumn replay`) must select an
        // early one-shot: rebuild the app, replay the recorded request, print a
        // verdict, exit. The runtime effect ends in `process::exit`, so — like
        // the migrate one-shot above — the *dispatch decision* and the
        // *offline contract* are locked structurally here, and the behaviour is
        // exercised end-to-end by `failure_capsule_end_to_end`.
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let run_start = source.find("pub async fn run(self)").expect("run() exists");
        let run_end = source
            .find("async fn run_build_mode(self)")
            .expect("build mode follows run()");
        let run_body = source
            .get(run_start..run_end)
            .expect("run() precedes build mode");

        let dispatch = run_body
            .find("if is_replay_mode()")
            .expect("run() dispatches the replay one-shot");
        let server_start = run_body
            .find("let Self {")
            .expect("run() destructures self to start the server");
        assert!(
            dispatch < server_start,
            "AUTUMN_REPLAY_CAPSULE must be handled before the server-start path"
        );
        let replay_branch = run_body
            .get(dispatch..server_start)
            .expect("the dispatch precedes server start");
        assert!(
            replay_branch.contains("self.run_replay_mode(capsule_path).await;")
                && replay_branch.contains("return;"),
            "the replay one-shot must run then return before server start"
        );

        let handler = replay_mode_source(&source);
        assert!(
            handler.contains("std::process::exit("),
            "the replay handler exits with the verdict's code"
        );
        assert!(
            !handler.contains("axum::serve"),
            "the replay one-shot must never start the server"
        );

        // It rebuilds the real thing: same router builder, same state
        // initializers, same policy registrations as the serving path — a
        // replay against a stripped-down router would not be a replay (R6).
        assert!(
            handler.contains("crate::capsule::load_capsule(")
                && handler.contains("crate::capsule::execute(")
                && handler.contains("crate::capsule::print_verdict("),
            "the replay handler loads the capsule, executes it and prints a verdict"
        );
        assert!(
            handler.contains("crate::router::try_build_router_inner(")
                && handler.contains("run_state_initializers(state_initializers, &state);")
                && handler.contains("register(state.policy_registry());"),
            "the replay handler must rebuild the app's real router and state"
        );

        // F15: replay is offline. No migrations, no job runtime, no scheduler,
        // no external session/cache backend — and it must not arm capture
        // against the stub pool.
        assert!(
            !handler.contains("setup_database(")
                && !handler.contains("initialize_job_runtime")
                && !handler.contains("start_task_scheduler")
                && !handler.contains("preflight_storage("),
            "replay must not run migrations, job workers or storage preflight"
        );
        assert!(
            handler.contains("force_offline_replay_config(&mut config);"),
            "the replay handler must force the offline configuration knobs"
        );

        let forcer_start = source
            .find("fn force_offline_replay_config(")
            .expect("the offline-knob helper exists");
        let forcer = source
            .get(forcer_start..forcer_start.saturating_add(2_000))
            .unwrap_or_default();
        assert!(
            forcer.contains("SessionBackend::Memory"),
            "replay must force the in-memory session store (no Redis)"
        );
        assert!(
            forcer.contains("failure_capture.enabled = false"),
            "replay must not capture a capsule of the replay itself"
        );
    }

    /// A replay must not reach anything the capsule does not contain. Three
    /// live-service escapes are closed here, in the same structural style as
    /// the session store below: the outbound HTTP client (a handler that calls
    /// a third party would otherwise call the *real* one), the channels backend
    /// (whose Redis form spawns a publisher and a listener against the
    /// application's live fan-out as soon as the state is built), and the
    /// request timeout (real tokio timers, so a debugger breakpoint would
    /// cancel the handler and print a mismatch that never happened).
    #[cfg(feature = "reporting")]
    #[test]
    fn replay_reaches_nothing_outside_the_capsule() {
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let handler = replay_mode_source(&source);

        assert!(
            handler.contains("crate::http_client::block_outbound_for_replay();"),
            "the replay handler must block outbound HTTP before rebuilding the app"
        );
        assert!(
            handler.contains("channels_backend: _replay_ignores_custom_channels_backend,"),
            "the replay handler must drop a custom channels backend, which outranks the \
             forced in-process one"
        );
        assert!(
            !handler.contains("            channels_backend,\n"),
            "the replay handler must not forward the builder's channels backend"
        );

        let forcer_start = source
            .find("fn force_offline_replay_config(")
            .expect("the offline-knob helper exists");
        // Wide enough for the whole helper: it forces every config-driven
        // store the request path can reach, so the window has to outgrow the
        // list rather than the list being trimmed to the window.
        let forcer = source
            .get(forcer_start..forcer_start.saturating_add(6_000))
            .unwrap_or_default();
        assert!(
            forcer.contains("config.channels.backend = crate::config::ChannelBackend::InProcess;"),
            "replay must force the in-process channels backend (no Redis fan-out)"
        );
        assert!(
            forcer.contains("config.server.timeouts.request_timeout_ms = None;"),
            "replay must clear the wall-clock request deadline"
        );
    }

    /// The knobs the helper forces, checked on a real configuration rather than
    /// on its source: everything a replay must not honour, in one place.
    #[cfg(feature = "reporting")]
    #[test]
    fn the_offline_replay_config_reaches_no_live_service() {
        let mut config = AutumnConfig::default();
        config.failure_capture.enabled = true;
        config.session.backend = crate::session::SessionBackend::Redis;
        config.channels.backend = crate::config::ChannelBackend::Redis;
        config.server.timeouts.request_timeout_ms = Some(30_000);
        // Every remaining config-driven store the request path can reach, set
        // the way a production recording would have had them.
        config.security.rate_limit.backend = crate::security::config::RateLimitBackend::Redis;
        config.idempotency.backend = crate::config::IdempotencyBackend::Redis;
        config.security.submit_token.backend = Some(crate::config::IdempotencyBackend::Redis);
        config.security.webhooks.replay.backend = crate::webhook::WebhookReplayBackend::Redis;
        config.cache.backend = crate::config::CacheBackend::Redis;
        config.jobs.backend = "redis".to_owned();

        force_offline_replay_config(&mut config);

        assert!(
            !config.failure_capture.enabled,
            "a replay must not capture a capsule of itself"
        );
        assert_eq!(
            config.session.backend,
            crate::session::SessionBackend::Memory
        );
        assert!(config.session.allow_memory_in_production);
        assert_eq!(
            config.channels.backend,
            crate::config::ChannelBackend::InProcess,
            "a replayed app must not dial the application's Redis fan-out"
        );
        assert_eq!(
            config.server.timeouts.request_timeout_ms, None,
            "a deterministic offline replay has no wall-clock deadline — and a \
             breakpoint held in a debugger must not cancel the handler"
        );
        // The middleware stores. Each of these is *written* by a replayed
        // request, so leaving any of them pointed at the recording
        // deployment's Redis would make diagnosing a failure mutate live state.
        assert_eq!(
            config.security.rate_limit.backend,
            crate::security::config::RateLimitBackend::Memory,
            "a replay must not consume the deployment's shared rate-limit budget"
        );
        assert_eq!(
            config.idempotency.backend,
            crate::config::IdempotencyBackend::Memory,
            "a replay must not take an idempotency key or its in-flight lock"
        );
        assert!(config.idempotency.allow_memory_in_production);
        assert_eq!(
            config.security.submit_token.backend,
            Some(crate::config::IdempotencyBackend::Memory),
            "submit tokens inherit the idempotency backend when unset, so a replay \
             must pin them explicitly rather than rely on that inheritance"
        );
        assert_eq!(
            config.security.webhooks.replay.backend,
            crate::webhook::WebhookReplayBackend::Memory,
            "a replay must not insert or delete webhook replay-protection keys"
        );
        assert!(config.security.webhooks.replay.allow_memory_in_production);
        assert_eq!(
            config.cache.backend,
            crate::config::CacheBackend::Memory,
            "a `#[cached]` handler must not read or populate the shared cache"
        );
        assert_eq!(
            config.jobs.backend, "local",
            "a job enqueued during a replay must not reach a queue a live worker drains"
        );
    }

    /// Forcing `session.backend = memory` is not enough on its own: a store
    /// installed with `with_session_store(...)` *outranks* the config in
    /// `apply_session_layer`, so passing it through would let a replay dial —
    /// and mutate — the application's live Redis or database session backend,
    /// or 503 when that backend is unreachable. The replay router must be built
    /// with no custom store so the forced memory backend is what applies.
    #[cfg(feature = "reporting")]
    #[test]
    fn replay_never_uses_the_applications_custom_session_store() {
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let handler = replay_mode_source(&source);

        assert!(
            handler.contains("session_store: None,"),
            "the replay router context must be built with no custom session store"
        );
        assert!(
            !handler.contains("            session_store,"),
            "the replay handler must not forward the builder's session store; \
             a custom store outranks the forced memory backend"
        );
    }

    #[cfg(feature = "reporting")]
    #[test]
    fn replay_never_invokes_a_custom_config_loader() {
        // Normalized like the other source assertions: a Windows checkout
        // (`core.autocrlf`) hands `include_str!` CRLF line endings, and the
        // `\n`-embedding pattern below would never match.
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let handler = replay_mode_source(&source);

        assert!(
            handler.contains("_replay_ignores_custom_config_loader = config_loader_factory"),
            "the replay handler must drop the builder's config loader — the documented \
             implementations call live services (Secrets Manager, Vault, Consul, HTTP)"
        );
        assert!(
            handler.contains("load_config_and_telemetry(\n            None,"),
            "replay must load configuration through the default local path, never a \
             custom loader"
        );
    }

    #[cfg(feature = "reporting")]
    #[test]
    fn replay_mode_reads_capsule_env_var() {
        temp_env::with_var(
            "AUTUMN_REPLAY_CAPSULE",
            Some("tmp/capsules/abc.json"),
            || {
                assert!(is_replay_mode(), "a capsule path selects the replay path");
                assert_eq!(
                    replay_capsule_from_env().as_deref(),
                    Some("tmp/capsules/abc.json")
                );
            },
        );
        temp_env::with_var("AUTUMN_REPLAY_CAPSULE", Some("  spaced.json  "), || {
            assert_eq!(
                replay_capsule_from_env().as_deref(),
                Some("spaced.json"),
                "a shell-quoted path must be trimmed"
            );
        });
        temp_env::with_var("AUTUMN_REPLAY_CAPSULE", Some("   "), || {
            assert!(
                !is_replay_mode(),
                "a blank value must fall through to the normal boot path"
            );
        });
        temp_env::with_var("AUTUMN_REPLAY_CAPSULE", None::<&str>, || {
            assert!(!is_replay_mode(), "unset must not select the replay path");
        });
    }

    #[cfg(feature = "db")]
    #[test]
    fn hooked_repository_apps_include_hook_queue_framework_migration() {
        let migrations = migrations_with_repository_framework_migrations(
            vec![("app", APP_TEST_MIGRATIONS)],
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
            vec![("app", APP_TEST_MIGRATIONS)],
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
    fn migration_names(migrations: &[(&str, crate::migrate::EmbeddedMigrations)]) -> Vec<String> {
        use diesel::migration::{Migration, MigrationSource as _};
        use diesel::pg::Pg;

        migrations
            .iter()
            .flat_map(|(_, source)| {
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
            vec![("app", crate::migrate::FRAMEWORK_MIGRATIONS)],
            true,
            true,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        );

        // The migration names the shard apply loop will actually run: every set
        // that is not the control framework set (which gets stripped on shards).
        let shard_names: Vec<String> = migrations
            .iter()
            .filter(|(_, set)| !migration_set_is_control_framework(set))
            .flat_map(|(_, set)| {
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
    fn plugin_migrations_registers_alongside_app_migrations() {
        const APP_MIGRATIONS: crate::migrate::EmbeddedMigrations =
            diesel_migrations::embed_migrations!("../examples/todo-app/migrations");
        const PLUGIN_MIGRATIONS: crate::migrate::EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_ok");

        let builder = app()
            .migrations(APP_MIGRATIONS)
            .plugin_migrations("test-plugin", PLUGIN_MIGRATIONS);

        let names = migration_names(&builder.migrations);
        assert!(
            names.iter().any(|n| n == "00000000000000_create_todos"),
            "app-registered migrations must still be applied: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "20260101000000_create_gizmos"),
            "plugin-registered migrations must be applied too: {names:?}"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn plugin_migrations_registration_never_panics_on_version_collision() {
        // The shape this guards against: an app's own first migration and a
        // plugin's migration both using the placeholder version
        // (`00000000000000`) with different content — exactly what
        // `examples/todo-app` hits against the framework's legacy
        // `create_api_tokens` migration. Registration must always succeed; the
        // collision is resolved at apply time instead (see
        // `compute_migration_disambiguation`'s own tests). Rejecting it here would
        // leave an app unable to use a plugin until someone renamed a migration in
        // a dependency they may not control.
        const APP_MIGRATIONS: crate::migrate::EmbeddedMigrations =
            diesel_migrations::embed_migrations!("../examples/todo-app/migrations");
        const COLLIDING_PLUGIN_MIGRATIONS: crate::migrate::EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_collision");

        let builder = app()
            .migrations(APP_MIGRATIONS)
            .plugin_migrations("test-plugin", COLLIDING_PLUGIN_MIGRATIONS);

        let names = migration_names(&builder.migrations);
        assert!(
            names.iter().any(|n| n == "00000000000000_create_todos"),
            "the app's own colliding migration must still be registered: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "00000000000000_create_gadgets"),
            "the plugin's colliding migration must still be registered: {names:?}"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn plugin_migrations_does_not_panic_on_identical_resubmission() {
        // Registering the exact same set twice (e.g. two plugins that both
        // depend on a shared migrations bundle) reuses the same versions
        // AND full names — the intentional, harmless duplication case, not a
        // collision.
        const PLUGIN_MIGRATIONS: crate::migrate::EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_ok");

        let _ = app()
            .plugin_migrations("plugin-a", PLUGIN_MIGRATIONS)
            .plugin_migrations("plugin-b", PLUGIN_MIGRATIONS);
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
            seo: crate::seo::SeoRouteDefaults::EMPTY,
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

    // ── exclude_static_routes_from_locale_prefix (issue #1251, Codex review) ──
    //
    // `#[static_get]` pre-rendering requests each route's single, unprefixed
    // path and rejects any non-2xx response; without this exclusion,
    // enabling `locale_prefix_enabled` would replace that path with a 308
    // redirect and break `autumn build` for every app with a static route.

    #[cfg(feature = "i18n")]
    fn static_meta(path: &'static str) -> crate::static_gen::StaticRouteMeta {
        crate::static_gen::StaticRouteMeta {
            path,
            name: "test_static_route",
            revalidate: None,
            params_fn: None,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
        }
    }

    #[cfg(feature = "i18n")]
    #[test]
    fn static_routes_are_excluded_when_locale_prefix_is_enabled() {
        let mut config = AutumnConfig::default();
        config.i18n.locale_prefix_enabled = true;
        let metas = vec![static_meta("/about"), static_meta("/pricing")];

        exclude_static_routes_from_locale_prefix(&mut config, &metas);

        assert_eq!(
            config.i18n.locale_prefix_exclude_exact,
            vec!["/about".to_owned(), "/pricing".to_owned()],
            "static routes must be tracked as EXACT exclusions, not prefix \
             exclusions — see the sibling `static_route_exclusion_does_not_leak_into_a_dynamic_sibling` test"
        );
        assert!(
            config.i18n.locale_prefix_exclude.is_empty(),
            "must not write static route paths into the prefix-matched exclude list"
        );
    }

    #[cfg(feature = "i18n")]
    #[test]
    fn static_route_exclusion_is_a_noop_when_locale_prefix_is_disabled() {
        let mut config = AutumnConfig::default();
        assert!(!config.i18n.locale_prefix_enabled);
        let metas = vec![static_meta("/about")];

        exclude_static_routes_from_locale_prefix(&mut config, &metas);

        assert!(
            config.i18n.locale_prefix_exclude_exact.is_empty(),
            "must not touch the exact-exclude list when the feature is off"
        );
    }

    #[cfg(feature = "i18n")]
    #[test]
    fn static_route_exclusion_preserves_existing_exclude_entries() {
        let mut config = AutumnConfig::default();
        config.i18n.locale_prefix_enabled = true;
        config.i18n.locale_prefix_exclude = vec!["/api".to_owned()];
        config.i18n.locale_prefix_exclude_exact = vec!["/contact".to_owned()];
        let metas = vec![static_meta("/about")];

        exclude_static_routes_from_locale_prefix(&mut config, &metas);

        assert_eq!(
            config.i18n.locale_prefix_exclude,
            vec!["/api".to_owned()],
            "must not touch the user-configured prefix-exclude list"
        );
        assert_eq!(
            config.i18n.locale_prefix_exclude_exact,
            vec!["/contact".to_owned(), "/about".to_owned()]
        );
    }

    #[cfg(feature = "i18n")]
    #[test]
    fn static_route_exclusion_preserves_a_root_static_route_path_verbatim() {
        let mut config = AutumnConfig::default();
        config.i18n.locale_prefix_enabled = true;
        let metas = vec![static_meta("/")];

        exclude_static_routes_from_locale_prefix(&mut config, &metas);

        assert_eq!(
            config.i18n.locale_prefix_exclude_exact,
            vec!["/".to_owned()]
        );
    }

    /// Codex review (P1): `AppState` caches an `AutumnConfig` snapshot
    /// (`build_state` clones it in before `exclude_static_routes_from_locale_prefix`
    /// runs) — `run()`/`run_build_mode()` must re-`insert_extension` the
    /// mutated config afterward, or `tenancy_middleware` (which reads config
    /// via `state.extension::<AutumnConfig>()`) would keep seeing the stale,
    /// pre-exclusion copy and could misjudge whether a `/{locale}`-look-alike
    /// path was ever actually locale-prefixed.
    #[cfg(feature = "i18n")]
    #[test]
    fn appstate_config_extension_reflects_static_route_exclusions_after_refresh() {
        let mut config = AutumnConfig::default();
        config.i18n.locale_prefix_enabled = true;
        let state = crate::state::AppState::for_test();
        // Simulates `build_state`'s clone, captured BEFORE the exclusion.
        state.insert_extension(config.clone());

        let metas = vec![static_meta("/about")];
        exclude_static_routes_from_locale_prefix(&mut config, &metas);
        // The fix: re-insert the mutated config so the stored snapshot
        // matches what the router was actually built with.
        state.insert_extension(config.clone());

        let stored = state
            .extension::<AutumnConfig>()
            .expect("config extension must be installed");
        assert_eq!(
            stored.i18n.locale_prefix_exclude_exact,
            vec!["/about".to_owned()],
            "AppState's config snapshot must reflect the static-route exclusion, \
             not the stale pre-mutation copy"
        );
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
            plugin_config_roots,
            ..
        } = builder;

        let (loaded_config, _guard) = load_config_and_telemetry(
            config_loader_factory,
            telemetry_provider,
            plugin_config_roots,
        )
        .await;
        let env = crate::config::MockEnv::new().with(
            "AUTUMN_MANIFEST_DIR",
            project.path().to_str().expect("utf-8 path"),
        );
        let bundle = resolve_i18n_bundle(i18n_bundle, i18n_auto_load, &loaded_config, &env)
            .expect("bundle loaded from configured dir");

        assert_eq!(bundle.translate("en", "nav.home", &[]), "Loader Home");
    }

    /// #1384: `locale_prefix_enabled` is supported WITHOUT `.i18n()` /
    /// `.i18n_auto()` — the router builds its `/{locale}` nests straight from
    /// `I18nConfig` — so no `Bundle` exists in that shape. Column decoding
    /// still needs the app's default locale: without it a `default_locale =
    /// "fr"` app attributes every legacy plain-text value to the last-resort
    /// `"en"`, so a `/fr/...` request renders upgraded content as empty and a
    /// later write can persist it under the wrong locale.
    #[cfg(feature = "i18n")]
    #[test]
    fn locale_defaults_are_installed_even_with_no_bundle() {
        let mut config = AutumnConfig::default();
        config.i18n.default_locale = "fr".to_owned();
        config.i18n.supported_locales = vec!["fr".to_owned(), "en".to_owned()];
        let state = AppState::for_test();

        let layers = install_i18n_bundle_layer(Vec::new(), &state, None, &config.i18n);

        assert!(
            layers.is_empty(),
            "no bundle means no Extension and no ambient layer registration"
        );
        assert_eq!(
            &*crate::i18n::default_locale_snapshot(),
            "fr",
            "the configured default must reach column decoding without a bundle"
        );
        // A legacy plain-text column is now attributed to `fr`, so a `/fr/...`
        // request resolves it instead of rendering empty.
        let legacy = crate::i18n::Translated::decode_column(
            "Bonjour le monde",
            &crate::i18n::default_locale_snapshot(),
        );
        assert_eq!(legacy.get("fr"), Some("Bonjour le monde"));

        // Restore the framework default for the rest of this binary.
        crate::i18n::install_locale_defaults("en", vec!["en".to_owned()]);
    }

    /// #1384: drive the REAL wiring — `install_i18n_bundle_layer` +
    /// `try_build_router_inner` — and assert both the ambient locale and a
    /// `Translated` field resolve for a handler that takes NO locale argument.
    ///
    /// This pins the layer-ORDERING invariant the feature rests on: the bundle
    /// `Extension` must be registered first (outermost) so the ambient layer,
    /// registered second, can read it while negotiating. Swap the two pushes
    /// and the layer negotiates against an empty supported-list, pins every
    /// request to the default locale, and this test goes red — where a
    /// hand-built router stack would not notice.
    #[cfg(feature = "i18n")]
    #[tokio::test]
    async fn ambient_locale_layer_resolves_translatable_content_through_the_real_stack() {
        use tower::ServiceExt as _;

        async fn show() -> String {
            let title =
                crate::i18n::Translated::from_pairs([("en", "Hello world"), ("es", "Hola mundo")]);
            // No `Locale` parameter anywhere in this signature.
            format!(
                "{}|{}",
                title,
                crate::i18n::ambient_locale().unwrap_or_default()
            )
        }

        let mut config = AutumnConfig::default();
        config.i18n.supported_locales = vec!["en".to_owned(), "es".to_owned()];
        let bundle = Arc::new(crate::i18n::Bundle::from_messages(
            std::collections::HashMap::new(),
            &config.i18n,
        ));
        let state = AppState::for_test();
        let custom_layers =
            install_i18n_bundle_layer(Vec::new(), &state, Some(bundle), &config.i18n);

        let router = crate::router::try_build_router_inner(
            vec![Route {
                method: http::Method::GET,
                path: "/show",
                handler: axum::routing::get(show),
                name: "show",
                api_doc: crate::openapi::ApiDoc {
                    method: "GET",
                    path: "/show",
                    operation_id: "show",
                    success_status: 200,
                    ..Default::default()
                },
                repository: None,
                idempotency: crate::route::RouteIdempotency::Direct,
                timeout: crate::route::RouteTimeout::Inherit,
                seo: crate::seo::SeoRouteDefaults::EMPTY,
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
                declared_routes: Vec::new(),
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

        for (accept_language, expected) in [
            ("es", "Hola mundo|es"),
            // Untranslated: falls back through the configured chain to `en`.
            ("fr", "Hello world|en"),
        ] {
            let request = axum::http::Request::builder()
                .uri("/show")
                .header(http::header::ACCEPT_LANGUAGE, accept_language)
                .body(axum::body::Body::empty())
                .expect("request");
            let response = router.clone().oneshot(request).await.expect("response");
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .expect("body");
            assert_eq!(
                String::from_utf8(bytes.to_vec()).expect("utf-8"),
                expected,
                "Accept-Language: {accept_language}"
            );
        }
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
            &config.i18n,
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
                seo: crate::seo::SeoRouteDefaults::EMPTY,
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
                declared_routes: Vec::new(),
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
                seo: crate::seo::SeoRouteDefaults::EMPTY,
            }],
            &dist,
        )
        .await
        .expect("static render succeeds");

        let html = std::fs::read_to_string(dist.join("about/index.html")).expect("rendered html");
        assert_eq!(html, "Home");
    }

    // ── Warden 2026-09-04: `#[static_get]` × multi-tenancy fails closed ──────
    //
    // Hypothesis: an app with documented multi-tenancy (`[tenancy] enabled =
    // true`, any `source`) pre-renders a `#[static_get]` route that reads
    // tenant-scoped state — a per-tenant storefront via the `Tenant` extractor, or
    // a `#[repository]` query scoped by `CURRENT_TENANT`. `autumn build` or ISR
    // regeneration might silently resolve a missing or default tenant and bake
    // that tenant's response into the single `dist/` file every tenant then
    // shares: a cross-tenant read through a first-class feature composition, with
    // no app-level analogue.
    //
    // It does not happen. `render_static_routes` (`autumn build`) and ISR's
    // `regenerate_page` (`static_gen/middleware.rs`) both send a bare synthetic
    // request through the full router — path only, no `Host`, no tenant header, no
    // session, no `Authorization` — and every tenancy `source` in `tenancy.rs`
    // (`extract_tenant_from_parts_inner`) rejects with a non-2xx `AutumnError`
    // when its required signal is absent, rather than resolving a default tenant.
    // A non-2xx is `BuildError::NonSuccessStatus` to the build/ISR caller, so the
    // handler's output never reaches disk. Asserted per tenancy source, so a
    // future change that makes any one of them fail open is caught rather than
    // silently shipping a cross-tenant static-file leak.
    #[tokio::test]
    async fn static_get_route_reading_tenant_fails_closed_for_every_tenancy_source() {
        async fn tenant_page(tenant: crate::tenancy::Tenant) -> String {
            tenant.0
        }

        for source in ["header", "subdomain", "session", "jwt"] {
            let mut config = AutumnConfig::default();
            config.tenancy.enabled = true;
            config.tenancy.source = source.to_owned();
            // Exempt the route from `tenancy_middleware` itself (Codex review,
            // PR #2505) so the request reaches `tenant_page` and the assertion
            // below exercises `Tenant::from_request_parts`'s own fallback call to
            // `extract_tenant_from_parts`, not just the middleware's earlier call
            // to the same function. Without this, every synthetic build/ISR
            // request is non-public and `tenancy_middleware` rejects it before the
            // handler runs, so a regression making the extractor's fallback fail
            // open would go undetected on a route listed in
            // `[tenancy].public_paths` whose handler still reads `Tenant`
            // directly. The `#[public]` macro attribute is a compile-time
            // route-audit marker and has no effect on `tenancy_middleware`.
            config.tenancy.public_paths = vec!["/storefront".to_owned()];

            let state = AppState::for_test();
            state.insert_extension(config.clone());

            let router = crate::router::try_build_router_inner(
                vec![Route {
                    method: http::Method::GET,
                    path: "/storefront",
                    handler: axum::routing::get(tenant_page),
                    name: "tenant_page",
                    api_doc: crate::openapi::ApiDoc {
                        method: "GET",
                        path: "/storefront",
                        operation_id: "tenant_page",
                        success_status: 200,
                        ..Default::default()
                    },
                    repository: None,
                    idempotency: crate::route::RouteIdempotency::Direct,
                    timeout: crate::route::RouteTimeout::Inherit,
                    seo: crate::seo::SeoRouteDefaults::EMPTY,
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
                    declared_routes: Vec::new(),
                    custom_layers: Vec::new(),
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
            .unwrap_or_else(|e| panic!("tenancy source {source:?}: router builds: {e}"));

            let tmp = tempfile::tempdir().expect("dist parent");
            let dist = tmp.path().join("dist");

            let result = crate::static_gen::render_static_routes(
                router,
                &[crate::static_gen::StaticRouteMeta {
                    path: "/storefront",
                    name: "tenant_page",
                    revalidate: None,
                    params_fn: None,
                    seo: crate::seo::SeoRouteDefaults::EMPTY,
                }],
                &dist,
            )
            .await;

            assert!(
                result.is_err(),
                "tenancy source {source:?}: a #[static_get] route reading `Tenant` must fail \
                 the build rather than silently bake a default/missing tenant's response into \
                 a dist/ file every tenant's requests would then share"
            );
            assert!(
                !dist.join("storefront/index.html").exists(),
                "tenancy source {source:?}: no file must be written when tenant resolution fails"
            );
        }
    }

    // ── Warden 2026-09-04 (Codex review, PR #2505): a failed rebuild does NOT
    // invalidate a pre-existing static file ──────────────────────────────────
    //
    // The test above starts from an empty `dist/`, so "no file is written on
    // failure" proves only that a fresh build never bakes in a cross-tenant
    // response. It says nothing about a `dist/<route>/index.html` left from an
    // earlier successful render. `render_static_routes` stages into a sibling
    // `dist.staging` directory and swaps it in only once every route rendered
    // successfully: the loop over `results` in `build.rs` returns on the first
    // `Err`, before the atomic remove-and-rename, so a failure leaves the existing
    // file untouched. ISR's `regenerate_page` has the same shape — it returns
    // `Err` before any `std::fs::write`/`rename` on a non-2xx response — so a
    // repeatedly-failing revalidation serves the same stale file forever, which is
    // the documented stale-while-revalidate contract.
    //
    // This does not reopen the cross-tenant hypothesis: nothing in Autumn writes a
    // tenant-scoped response into `dist/` without a resolved tenant (see the sweep
    // above). The realistic way a tenant-mismatched file lands there is
    // operational — building with a different `[tenancy]` config than the one
    // serving requests. But once such a file exists, this test shows Autumn has no
    // mechanism to detect, invalidate, or expire it: a later tenant-resolution
    // failure preserves it indefinitely, with no operator-visible signal beyond a
    // log line. Documented as a known limitation rather than left implicit.
    #[tokio::test]
    async fn failed_rebuild_leaves_preexisting_static_file_untouched() {
        async fn tenant_page(tenant: crate::tenancy::Tenant) -> String {
            tenant.0
        }

        let mut config = AutumnConfig::default();
        config.tenancy.enabled = true;
        config.tenancy.source = "header".to_owned();
        config.tenancy.public_paths = vec!["/storefront".to_owned()];

        let state = AppState::for_test();
        state.insert_extension(config.clone());

        let router = crate::router::try_build_router_inner(
            vec![Route {
                method: http::Method::GET,
                path: "/storefront",
                handler: axum::routing::get(tenant_page),
                name: "tenant_page",
                api_doc: crate::openapi::ApiDoc {
                    method: "GET",
                    path: "/storefront",
                    operation_id: "tenant_page",
                    success_status: 200,
                    ..Default::default()
                },
                repository: None,
                idempotency: crate::route::RouteIdempotency::Direct,
                timeout: crate::route::RouteTimeout::Inherit,
                seo: crate::seo::SeoRouteDefaults::EMPTY,
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
                declared_routes: Vec::new(),
                custom_layers: Vec::new(),
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

        // Seed `dist/` as if an earlier, successful build/regeneration had
        // captured tenant A's response (the operationally-realistic route:
        // a build/serve `[tenancy]` config mismatch, not anything this
        // framework's own request handling can produce — see the sweep
        // above).
        std::fs::create_dir_all(dist.join("storefront")).expect("mkdir storefront");
        let sentinel = "tenant-a-sentinel-warden-2026-09-04";
        std::fs::write(dist.join("storefront/index.html"), sentinel).expect("seed stale file");
        let mut seed_routes = std::collections::HashMap::new();
        seed_routes.insert(
            "/storefront".to_owned(),
            crate::static_gen::ManifestEntry::new("storefront/index.html".to_owned()),
        );
        let manifest = crate::static_gen::StaticManifest::new(seed_routes);
        std::fs::write(
            dist.join("manifest.json"),
            serde_json::to_string(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");

        let result = crate::static_gen::render_static_routes(
            router,
            &[crate::static_gen::StaticRouteMeta {
                path: "/storefront",
                name: "tenant_page",
                revalidate: None,
                params_fn: None,
                seo: crate::seo::SeoRouteDefaults::EMPTY,
            }],
            &dist,
        )
        .await;

        assert!(
            result.is_err(),
            "the rebuild must still fail closed (tenant resolution rejects the headerless \
             synthetic request), same as the fresh-dist case above"
        );

        let surviving = std::fs::read_to_string(dist.join("storefront/index.html"))
            .expect("the pre-existing file must still be present after a failed rebuild");
        assert_eq!(
            surviving, sentinel,
            "a failed rebuild must not silently alter or remove a pre-existing static file. \
             This is Autumn's documented stale-while-revalidate/atomic-swap design working as \
             intended, but it also means a tenant-mismatched file that reached dist/ by some \
             other means (an operational build/serve config mismatch, not a code path in this \
             framework) is served indefinitely with no automatic invalidation — see \
             docs/security/2026-09-04-static-gen-tenancy-fails-closed/README.md"
        );
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
                version: 1,
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
            true,
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
                version: 1,
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
            true,
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
        let router = test_router_with_config(vec![test_get_route("/dummy", "dummy")], &config);

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
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        }];
        let router = test_router(post_routes);

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
                seo: crate::seo::SeoRouteDefaults::EMPTY,
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
                seo: crate::seo::SeoRouteDefaults::EMPTY,
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

        let manifest = crate::static_gen::StaticManifest::new(HashMap::from([(
            "/docs".to_owned(),
            crate::static_gen::ManifestEntry::new("docs/index.html".to_owned()),
        )]))
        .with_generated_at("2026-03-27T00:00:00Z");
        let json = serde_json::to_string(&manifest).expect("serialize");
        std::fs::write(dist.join("manifest.json"), json).expect("write manifest");

        // No dynamic route for /docs — only a static file.
        let config = AutumnConfig::default();
        let state = AppState {
            health_detailed: true,
            ..AppState::test_default()
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

    /// #1832 through the composed `build_router_with_static` stack rather than
    /// the router tests' narrower helper: the manifest's recorded type must
    /// survive the security-headers layer and arrive alongside
    /// `X-Content-Type-Options: nosniff` — which is precisely why it has to be
    /// right. `/feed` is extensionless and stored as `feed/index.html`, so both
    /// legacy clues say `text/html`; only the recorded value makes it RSS.
    #[tokio::test]
    async fn build_router_serves_recorded_content_type_with_nosniff() {
        use std::collections::HashMap;

        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(dist.join("feed")).expect("mkdir");
        std::fs::write(dist.join("feed/index.html"), "<rss/>").expect("write");

        let manifest = crate::static_gen::StaticManifest::new(HashMap::from([(
            "/feed".to_owned(),
            crate::static_gen::ManifestEntry::new("feed/index.html".to_owned())
                .with_content_type(Some("application/rss+xml".to_owned())),
        )]));
        std::fs::write(
            dist.join("manifest.json"),
            serde_json::to_string(&manifest).expect("serialize"),
        )
        .expect("write manifest");

        let config = AutumnConfig::default();
        let router = crate::router::build_router_with_static(
            Vec::new(),
            &config,
            AppState::for_test(),
            Some(dist.as_path()),
        );

        let response = router
            .oneshot(Request::builder().uri("/feed").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/rss+xml"),
            "the recorded type must survive the full composed stack"
        );
        assert_eq!(
            response
                .headers()
                .get("x-content-type-options")
                .and_then(|v| v.to_str().ok()),
            Some("nosniff"),
            "nosniff is why the recorded type has to be correct: the browser \
             will not second-guess it"
        );
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
                    seo: crate::seo::SeoRouteDefaults::EMPTY,
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
                    seo: crate::seo::SeoRouteDefaults::EMPTY,
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
                    seo: crate::seo::SeoRouteDefaults::EMPTY,
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
            seo: crate::seo::SeoRouteDefaults::EMPTY,
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
            health_detailed: true,
            ..AppState::test_default()
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
            health_detailed: true,
            ..AppState::test_default()
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
            health_detailed: true,
            ..AppState::test_default()
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
            health_detailed: true,
            ..AppState::test_default()
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
            health_detailed: true,
            ..AppState::test_default()
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
        let start = state.monotonic();
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
        let start = state.monotonic();
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
        let start = state.monotonic();
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

    // AC5 plumbing (#1526): `with_story_gallery` stores the gallery on the
    // builder, and `install_story_registry` — the single install step shared
    // by both the run and build/SSG state-construction paths — publishes it
    // as the StoryRegistry extension the `/_stories` handlers read.
    #[cfg(feature = "maud")]
    #[test]
    fn with_story_gallery_installs_story_registry_extension() {
        let builder = crate::app().with_story_gallery(crate::stories::StoryGallery::builtin());
        let gallery = builder
            .story_gallery
            .expect("with_story_gallery must store the gallery on the builder");
        let expected_count = gallery.stories().len();
        assert!(expected_count > 0, "builtin gallery must not be empty");

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
        install_story_registry(&state, Some(gallery));
        let registry = state
            .extension::<crate::stories::StoryRegistry>()
            .expect("install_story_registry must publish the StoryRegistry extension");
        assert_eq!(
            registry.stories().len(),
            expected_count,
            "every registered story must reach the state extension"
        );

        // Without a registered gallery no extension is installed: the
        // handlers fall back to the empty default and serve the empty state.
        let bare_state = build_state(
            &config,
            #[cfg(feature = "db")]
            None,
            #[cfg(feature = "db")]
            None,
            #[cfg(feature = "ws")]
            None,
        );
        install_story_registry(&bare_state, None);
        assert!(
            bare_state
                .extension::<crate::stories::StoryRegistry>()
                .is_none(),
            "no gallery registered must mean no StoryRegistry extension"
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
