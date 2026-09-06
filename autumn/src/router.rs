//! Router construction and configuration.
//!
//! This module handles assembling the final [`axum::Router`] from the various
//! components configured in [`AppBuilder`](crate::app::AppBuilder), including
//! user routes, static files, middleware, error pages, and framework endpoints
//! like actuators and probes.

use std::sync::Arc;
use std::time::Duration;

use crate::app::ScopedGroup;
use crate::config::AutumnConfig;
#[cfg(feature = "maud")]
use crate::error_pages::{self, SharedRenderer};
use crate::idempotency::{IdempotencyLayer, IdempotencyStore, MemoryIdempotencyStore};
use crate::middleware::RequestIdLayer;
use crate::middleware::dev;
use crate::middleware::exception_filter::{
    ExceptionFilter, ExceptionFilterLayer, ProblemDetailsFilter,
};
use crate::route::Route;
use crate::state::AppState;
use axum::response::IntoResponse;
use http::{Request, StatusCode};
use thiserror::Error;

pub const DEFAULT_FAVICON_PATH: &str = "/favicon.ico";

/// Errors that can occur during the router build process.
///
/// These errors are typically fatal and represent configuration or routing
/// definition issues that must be fixed before the application can start.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RouterBuildError {
    /// The session backend configuration is invalid (e.g. Redis without a URL).
    #[error("invalid session backend configuration: {0}")]
    InvalidSessionBackend(#[from] crate::session::SessionBackendConfigError),
    /// The idempotency backend configuration is invalid.
    #[error("invalid idempotency backend configuration: {0}")]
    #[allow(dead_code)] // constructed only in the `redis` feature path
    InvalidIdempotencyBackend(String),
    /// The submit-token backend configuration is invalid for production — an
    /// explicit `[security.submit_token].backend = "memory"` cannot safely
    /// deduplicate submits across replicas. Mirrors the idempotency
    /// production-memory fail-fast.
    #[error("invalid submit-token backend configuration: {0}")]
    InvalidSubmitTokenBackend(String),
    /// A user-defined route conflicts with a framework-provided route.
    #[error("framework route overlap at {path}: {existing} conflicts with {incoming}")]
    FrameworkRouteOverlap {
        /// The HTTP path where the overlap occurred.
        path: String,
        /// The name of the existing framework route.
        existing: &'static str,
        /// The name of the incoming user route.
        incoming: &'static str,
    },
    /// An `OpenApiConfig` path (e.g. `openapi_json_path` or
    /// `swagger_ui_path`) is not a valid route path (must start with `/`
    /// and be non-empty).
    #[cfg(feature = "openapi")]
    #[error("invalid OpenAPI {field} path: {value:?} (must start with '/' and be non-empty)")]
    InvalidOpenApiPath {
        /// Which config field carried the invalid path.
        field: &'static str,
        /// The offending value from the user's config.
        value: String,
    },
    /// `openapi_json_path` and `swagger_ui_path` collide on the same
    /// URL. Mounting both would cause axum to panic on overlapping
    /// method routes at startup.
    #[cfg(feature = "openapi")]
    #[error(
        "openapi_json_path and swagger_ui_path both resolve to {path:?}; they must differ or `swagger_ui_path` must be `None`"
    )]
    DuplicateOpenApiPath {
        /// The path that both fields pointed at.
        path: String,
    },
    /// An `OpenAPI` mount path overlaps with an existing `GET` handler,
    /// which would panic at `axum::Router::merge` time.
    #[cfg(feature = "openapi")]
    #[error(
        "OpenAPI {field} path {path:?} collides with an existing GET route; choose a different `OpenApiConfig::{field}`"
    )]
    OpenApiPathCollision {
        /// Which config field carried the colliding path.
        field: &'static str,
        /// The colliding path.
        path: String,
    },
    /// A route is annotated with an API version that is not registered.
    #[error("route '{route_name}' uses unregistered API version '{version}'")]
    UnregisteredApiVersion { route_name: String, version: String },
    /// The MCP mount path (from [`AppBuilder::mount_mcp`](crate::app::AppBuilder::mount_mcp))
    /// is not a valid route path. axum requires paths to start with `/`, so an
    /// invalid path is surfaced here rather than panicking at mount time.
    #[cfg(feature = "mcp")]
    #[error("invalid MCP mount path: {value:?} (must start with '/' and be non-empty)")]
    InvalidMcpPath {
        /// The offending mount path.
        value: String,
    },
    /// The MCP mount path collides with an existing application route at the
    /// same path. Mounting the MCP endpoint there would panic at
    /// `axum::Router::merge` time on overlapping method routes, so this is
    /// surfaced as a recoverable error instead.
    #[cfg(feature = "mcp")]
    #[error(
        "MCP mount path {path:?} collides with an existing {method} route; choose a different `mount_mcp` path"
    )]
    McpPathCollision {
        /// The colliding mount path.
        path: String,
        /// The HTTP method of the existing route at that path.
        method: String,
    },
    /// Two user- or plugin-registered routes resolve to the same
    /// `(method, path)` after scope-prefix resolution. Mounting both would
    /// panic inside `axum::routing::MethodRouter::merge` at startup on
    /// overlapping method routes (issue #1012), so the collision preflight
    /// surfaces it as a recoverable [`RouterBuildError`] BEFORE any router
    /// is mounted and names both handlers so the offending call sites are
    /// obvious in the log.
    ///
    /// Opaque routers registered via
    /// [`AppBuilder::merge`](crate::app::AppBuilder::merge) or
    /// [`AppBuilder::nest`](crate::app::AppBuilder::nest) are NOT introspectable
    /// through axum's public API, so a collision that involves one of those
    /// routers cannot be detected up front and will still surface as an axum
    /// startup panic — the preflight emits a `tracing::warn!` in that case so
    /// operators know the check was skipped (mirrors the existing OpenAPI/MCP
    /// merge-router warnings).
    #[error(
        "duplicate user route: {existing:?} and {incoming:?} both resolve to {method} {path:?}; \
         choose a different path for one of them or remove the duplicate registration"
    )]
    DuplicateUserRoute {
        /// The HTTP method both handlers registered.
        method: String,
        /// The URL path both handlers registered (post scope-prefix resolution).
        path: String,
        /// The `route.name` of the first (already-seen) handler.
        existing: String,
        /// The `route.name` of the second (duplicate) handler that triggered
        /// the collision.
        incoming: String,
    },
    /// Two routers were registered at the SAME nest prefix via
    /// [`AppBuilder::nest`](crate::app::AppBuilder::nest).
    ///
    /// Every nested router is given a fallback before it is mounted
    /// (`mount_raw_routers`), and axum cannot merge two method routers that
    /// both have one — nesting the second at a prefix the first already owns
    /// panics with *"Cannot merge two `MethodRouter`s that both have a
    /// fallback"* while the router is built.
    ///
    /// The declared-route preflight cannot catch this: two sandboxed plugins
    /// sharing a prefix while declaring *disjoint* routes have no route
    /// collision at all, and are accepted right up until the second nest
    /// panics. A prefix is therefore owned by whoever nests at it first.
    #[error(
        "two routers are nested at {prefix:?} ({owners}); axum gives each nested router a fallback \
         and cannot merge two that both have one, so the second panics at startup — give each \
         router its own prefix"
    )]
    DuplicateNestPrefix {
        /// The prefix both routers were nested at.
        prefix: String,
        /// Who claims the prefix, as far as the declared routes reveal.
        owners: String,
    },
    /// Two user- or plugin-registered routes normalize to the SAME Axum path
    /// shape but use DIFFERENT exact path templates — e.g. their capture names
    /// differ (`/users/{id}` vs `/users/{slug}`) or a normal capture meets a
    /// catch-all at the same position (`/u/{id}` vs `/u/{*rest}`).
    ///
    /// axum's matchit router rejects the second template as a route conflict
    /// *before* method-router merging, so — unlike an exact-duplicate path,
    /// which axum happily merges across distinct HTTP methods
    /// ([`DuplicateUserRoute`](Self::DuplicateUserRoute)) — these two templates
    /// can never coexist REGARDLESS of method. Issue #1012 surfaces the clash
    /// here (naming both handlers and both original templates) instead of
    /// letting the matchit conflict panic inside `Router::route` at startup.
    ///
    /// Opaque `AppBuilder::merge` / `AppBuilder::nest` routers are exempt for
    /// the same reason as [`DuplicateUserRoute`](Self::DuplicateUserRoute).
    #[error(
        "conflicting route shapes: {existing:?} ({existing_path:?}) and {incoming:?} ({incoming_path:?}) \
         resolve to the same Axum path shape but use different path templates; axum's matchit router \
         rejects this as a route conflict regardless of HTTP method — rename the captures so both use the \
         same template, or make their static paths distinct"
    )]
    ConflictingRouteShape {
        /// The `route.name` of the first (already-seen) handler.
        existing: String,
        /// The original path template registered by the first handler.
        existing_path: String,
        /// The `route.name` of the second handler that triggered the conflict.
        incoming: String,
        /// The original path template registered by the second handler.
        incoming_path: String,
    },
    /// Locale-prefix routing (issue #1251) would generate a path that
    /// collides with another route already registered at that exact path —
    /// e.g. an app defines both `/foo` and `/en/foo` while `en` is a
    /// supported locale: the bare-path redirect (mounted at every
    /// locale-prefix-eligible path, claiming every HTTP method) already owns
    /// `/en/foo`, so nesting `/foo`'s content under `/en` collides and axum
    /// panics on the overlapping route at router-construction time. Detected
    /// and surfaced as a build error instead (Codex review).
    #[cfg(feature = "i18n")]
    #[error(
        "locale-prefix routing generates {generated:?} (locale {locale:?} + route {path:?}), \
         which collides with another route already registered at that path — rename one of the \
         routes, or exclude it via `[i18n] locale_prefix_exclude`"
    )]
    LocalePrefixPathCollision {
        /// The supported locale whose nest produced the collision.
        locale: String,
        /// The original, locale-prefix-eligible route path that got nested.
        path: String,
        /// The resulting generated path (`/{locale}{path}`) that collides.
        generated: String,
    },
}

/// Build the fully-configured Axum router from routes, config, and state.
///
/// Extracted from `AppBuilder::run` so the router construction logic is
/// testable without binding a real TCP listener.
///
/// # Panics
///
/// Panics when framework router assembly encounters invalid configuration.
/// Use [`try_build_router`] to handle configuration errors explicitly.
#[allow(dead_code)]
pub fn build_router(
    route_list: Vec<Route>,
    config: &AutumnConfig,
    state: AppState,
) -> axum::Router {
    try_build_router(route_list, config, state)
        .unwrap_or_else(|error| panic!("invalid router configuration: {error}"))
}

/// Checked variant of [`build_router`] that returns configuration errors
/// instead of panicking.
///
/// # Errors
///
/// Returns [`RouterBuildError`] when router assembly encounters invalid
/// framework configuration, such as an unusable session backend.
pub struct RouterContext {
    pub exception_filters: Vec<Arc<dyn ExceptionFilter>>,
    pub scoped_groups: Vec<ScopedGroup>,
    pub merge_routers: Vec<axum::Router<AppState>>,
    pub nest_routers: Vec<(String, axum::Router<AppState>)>,
    /// Route tables declared for otherwise-opaque `nest` mounts via
    /// [`AppBuilder::declare_plugin_routes`](crate::app::AppBuilder::declare_plugin_routes).
    ///
    /// A nested router is normally opaque — axum exposes no way to enumerate
    /// it — which is why the duplicate-route preflight has to skip it. A plugin
    /// that declares its routes hands over that table anyway, and for a
    /// sandboxed plugin the manifest *is* the table. Carrying it here lets
    /// [`reject_duplicate_user_routes`] check a declared mount against the
    /// application's own routes, so an artifact declaring a path the app
    /// already serves is a typed startup error instead of a panic inside
    /// `Router::nest`.
    pub declared_routes: Vec<crate::route_listing::RouteInfo>,
    /// Custom Tower layers registered via
    /// [`AppBuilder::layer`](crate::app::AppBuilder::layer). Applied inside
    /// [`RequestIdLayer`] and the session layer on the ingress path so user
    /// middleware observes the generated request ID and session context.
    ///
    /// **SSG/ISG mode trade-off**: when `dist_dir` is active, layers are
    /// moved outside the static-first middleware so they can process
    /// pre-rendered responses (e.g. compression).  As a side effect they also
    /// run *before* `RequestIdLayer`, session, `MetricsLayer`, and
    /// `ExceptionFilterLayer` for all requests (static and dynamic).  Layers
    /// that depend on extensions set by those framework layers — such as the
    /// request ID or session data — will not find them in SSG mode.
    pub custom_layers: Vec<crate::app::CustomLayerRegistration>,
    /// Pre-static gate layers registered via
    /// [`AppBuilder::static_gate`](crate::app::AppBuilder::static_gate).
    /// Applied as the **outermost** middleware — outside the session layer and
    /// ahead of the static-first middleware — so they can auth-gate / redirect
    /// a request before a cached SSG/ISG page is served. Unlike
    /// [`custom_layers`](Self::custom_layers), these always run in this
    /// outermost position in both static and fully-dynamic modes, and never
    /// see the session extension.
    pub static_gate_layers: Vec<crate::app::CustomLayerRegistration>,
    #[cfg(feature = "maud")]
    pub error_page_renderer: Option<SharedRenderer>,
    /// Custom session store installed via
    /// [`AppBuilder::with_session_store`](crate::app::AppBuilder::with_session_store).
    /// When `Some`, [`build_session_layer`](crate::session::build_session_layer)
    /// uses it directly and skips the config-driven backend selection.
    pub session_store: Option<Arc<dyn crate::session::BoxedSessionStore>>,
    /// `OpenAPI` generation configuration. When `Some`, the router mounts
    /// an `openapi.json` endpoint and (optionally) a Swagger UI page
    /// describing the application's routes.
    ///
    /// Gated behind the `openapi` feature.
    #[cfg(feature = "openapi")]
    pub openapi: Option<crate::openapi::OpenApiConfig>,
    /// MCP (Model Context Protocol) runtime config. When `Some`, the router
    /// mounts a Streamable-HTTP MCP endpoint that projects opted-in routes as
    /// agent-callable tools and dispatches `tools/call` through the real
    /// handler pipeline.
    ///
    /// Gated behind the `mcp` feature.
    #[cfg(feature = "mcp")]
    pub mcp: Option<crate::mcp::McpRuntime>,
}

/// Checked variant of [`build_router`] that returns configuration errors
/// instead of panicking.
///
/// # Errors
///
/// Returns [`RouterBuildError`] when router assembly encounters invalid
/// framework configuration, such as an unusable session backend.
pub fn try_build_router(
    route_list: Vec<Route>,
    config: &AutumnConfig,
    state: AppState,
) -> Result<axum::Router, RouterBuildError> {
    try_build_router_with_layers(route_list, config, state, Vec::new())
}

/// [`try_build_router`] plus caller-supplied app-wide Tower layers.
///
/// The layers are handed to the same [`RouterContext::custom_layers`] slot
/// that [`AppBuilder::layer`](crate::app::AppBuilder::layer) uses, so they
/// land in the identical stack position (inside `RequestId` and the session
/// layer, outside CSRF/CORS) and compose with the identical ordering
/// contract — the first registration is the outermost layer on ingress.
///
/// Exists for `SystemTest::layer` (feature `system-tests`, hence no intra-doc
/// link from this always-compiled module): a browser test that registers its
/// app's middleware must observe exactly the stack the real app serves,
/// otherwise the harness lies about what production does.
///
/// # Errors
///
/// Returns [`RouterBuildError`] when router assembly encounters invalid
/// framework configuration, such as an unusable session backend.
pub fn try_build_router_with_layers(
    route_list: Vec<Route>,
    config: &AutumnConfig,
    state: AppState,
    custom_layers: Vec<crate::app::CustomLayerRegistration>,
) -> Result<axum::Router, RouterBuildError> {
    let startup_barrier_state = state.clone();
    let router = try_build_router_inner(
        route_list,
        config,
        state,
        RouterContext {
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
    )?;
    Ok(apply_startup_barrier(
        router,
        config,
        &startup_barrier_state,
    ))
}

/// Build a router that includes user-supplied raw Axum routers.
///
/// Like [`build_router`], but also merges and nests additional raw
/// Axum routers. This is primarily useful for integration testing;
/// in production, use [`AppBuilder::merge`](crate::app::AppBuilder::merge) and [`AppBuilder::nest`](crate::app::AppBuilder::nest).
///
/// # Panics
///
/// Panics when framework router assembly encounters invalid configuration.
/// Use [`try_build_router_merged`] to handle configuration errors explicitly.
#[allow(dead_code)]
pub fn build_router_merged(
    route_list: Vec<Route>,
    config: &AutumnConfig,
    state: AppState,
    merge_routers: Vec<axum::Router<AppState>>,
    nest_routers: Vec<(String, axum::Router<AppState>)>,
) -> axum::Router {
    try_build_router_merged(route_list, config, state, merge_routers, nest_routers)
        .unwrap_or_else(|error| panic!("invalid router configuration: {error}"))
}

/// Checked variant of [`build_router_merged`] that returns configuration
/// errors instead of panicking.
///
/// # Errors
///
/// Returns [`RouterBuildError`] when router assembly encounters invalid
/// framework configuration, such as an unusable session backend.
#[allow(dead_code)]
pub fn try_build_router_merged(
    route_list: Vec<Route>,
    config: &AutumnConfig,
    state: AppState,
    merge_routers: Vec<axum::Router<AppState>>,
    nest_routers: Vec<(String, axum::Router<AppState>)>,
) -> Result<axum::Router, RouterBuildError> {
    let startup_barrier_state = state.clone();
    let router = try_build_router_inner(
        route_list,
        config,
        state,
        RouterContext {
            exception_filters: Vec::new(),
            scoped_groups: Vec::new(),
            merge_routers,
            nest_routers,
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
    )?;
    Ok(apply_startup_barrier(
        router,
        config,
        &startup_barrier_state,
    ))
}

pub fn try_build_router_inner(
    route_list: Vec<Route>,
    config: &AutumnConfig,
    state: AppState,
    ctx: RouterContext,
) -> Result<axum::Router, RouterBuildError> {
    // Fully-dynamic path: no outer SecurityHeadersLayer is applied after this
    // returns, so build_router_pre_state applies it (outermost, wrapping the
    // gate).
    let router = build_router_pre_state(route_list, config, &state, ctx, None, false)?;
    Ok(router.with_state(state))
}

/// Build a probe-only router for the [`Worker`](crate::config::ProcessRole::Worker)
/// process role.
///
/// A worker replica runs job workers and the cron scheduler but serves no user
/// routes. It still binds the HTTP listener so orchestrators can supervise it,
/// exposing **only** the framework liveness/readiness/startup/health probes
/// (per `config.health.*`) and the actuator (`/actuator/*`, so `/actuator/jobs`
/// works). This mirrors how [`try_build_router_with_static_inner`] finalizes the
/// full router — same startup barrier and `with_state` — so probe/actuator
/// behavior is identical, only the user route table is absent.
///
/// # Errors
///
/// Returns [`RouterBuildError`] when the actuator prefix collides with a probe
/// path (the same guard the full build path applies).
pub fn try_build_probe_only_router(
    config: &AutumnConfig,
    state: AppState,
) -> Result<axum::Router, RouterBuildError> {
    let barrier_state = state.clone();
    // A worker replica serves no user routes, so nothing can shadow a probe.
    let no_user_routes = std::collections::HashSet::new();
    let (mounted_probe_paths, router) =
        mount_probe_endpoints(axum::Router::<AppState>::new(), config, &no_user_routes);
    let router = mount_actuator_endpoints(router, config, &mounted_probe_paths)?;
    let router = router.with_state(state);
    Ok(apply_startup_barrier(router, config, &barrier_state))
}

/// Prepared MCP exposure carried through `build_router_pre_state`: the mount
/// path, the derived tool catalog, and the optional whole-endpoint auth layer.
#[cfg(feature = "mcp")]
type McpPrepared = (
    String,
    Vec<crate::mcp::McpToolInfo>,
    Option<crate::mcp::McpEndpointLayer>,
);

/// Like [`try_build_router_inner`] but returns `Router<AppState>` before
/// [`with_state`](axum::Router::with_state) is called.  Used by
/// [`try_build_router_with_static_inner`] so that user layers and the static
/// file middleware can be applied to the typed router before state is baked in.
#[allow(clippy::too_many_lines)]
fn build_router_pre_state(
    route_list: Vec<Route>,
    config: &AutumnConfig,
    state: &AppState,
    #[cfg_attr(not(feature = "mcp"), allow(unused_mut))] mut ctx: RouterContext,
    // When custom_layers are extracted from ctx before this call (SSG path),
    // the caller pre-computes the flag so the idempotency selector still sees
    // the real layer list even though ctx.custom_layers is empty.
    opaque_app_layers_override: Option<bool>,
    // When true (SSG/ISG path), the `SecurityHeadersLayer` is NOT applied here:
    // `try_build_router_with_static_inner` applies a single one OUTSIDE the
    // static-first middleware (wrapping cached pages, dynamic misses, and the
    // gate), so applying it here too would double-apply it (which breaks CSP
    // nonces). In the fully-dynamic path this is `false` and the layer is
    // applied as the outermost framework layer below, wrapping the gate.
    defer_security_headers: bool,
) -> Result<axum::Router<AppState>, RouterBuildError> {
    // Verify registered API versions
    let versions = state.extension::<crate::app::RegisteredApiVersions>();
    let registered_versions: std::collections::HashSet<&str> = versions
        .as_ref()
        .map(|v| v.0.iter().map(|av| av.version.as_str()).collect())
        .unwrap_or_default();

    let check_route_version = |route: &Route| -> Result<(), RouterBuildError> {
        if let Some(version) = route
            .api_version
            .filter(|ver| !registered_versions.contains(*ver))
        {
            return Err(RouterBuildError::UnregisteredApiVersion {
                route_name: route.name.to_string(),
                version: version.to_string(),
            });
        }
        Ok(())
    };

    for route in &route_list {
        check_route_version(route)?;
    }
    for group in &ctx.scoped_groups {
        for route in &group.routes {
            check_route_version(route)?;
        }
    }

    // Fail fast when two user- or plugin-registered routes share a `(method,
    // path)`. `group_and_mount_routes` would hand the overlap to
    // `MethodRouter::merge`, which panics inside `Router::route` at startup
    // (#1012). Runs before the OpenAPI/MCP preflights so the error is always
    // `DuplicateUserRoute`, and before any mount so the failure is structured
    // rather than an axum panic.
    reject_duplicate_user_routes(
        &route_list,
        &ctx.scoped_groups,
        &ctx.merge_routers,
        &ctx.nest_routers,
        &ctx.declared_routes,
        config,
    )?;

    // Fail-fast if an OpenAPI mount path collides with a user or
    // framework GET route — axum panics on overlapping method routes,
    // so surface this as a recoverable error before we start merging.
    #[cfg(feature = "openapi")]
    reject_openapi_path_collisions(
        ctx.openapi.as_ref(),
        &route_list,
        &ctx.scoped_groups,
        &ctx.merge_routers,
        &ctx.nest_routers,
        config,
    )?;

    // Build the OpenAPI spec BEFORE moving the routes into axum, because
    // group_and_mount_routes consumes the Route list.
    #[cfg(feature = "openapi")]
    let openapi_router = build_openapi_router(
        &route_list,
        &ctx.scoped_groups,
        ctx.openapi.as_ref(),
        &config.session.cookie_name,
        versions.as_ref().map_or(&[], |v| v.0.as_slice()),
    )?;

    // Prepare MCP exposure *before* `route_list` is moved into axum below.
    // Validate the mount path up front (a typo like `"mcp"` surfaces as a
    // recoverable error, mirroring the OpenAPI path validation, instead of an
    // axum panic), derive the tool catalog, and carry the optional endpoint
    // auth layer to be applied once the router is assembled.
    #[cfg(feature = "mcp")]
    let mcp_prepared: Option<McpPrepared> = if let Some(rt) = ctx.mcp.take() {
        let path = rt.mount_path.as_str();
        // The mount path must be one static endpoint. Reject empty,
        // non-absolute, doubled-slash, and dynamic (`{capture}` / `{*rest}`)
        // paths so MCP cannot shadow a path class, and so the collision
        // preflight reserves the exact URL it matches. Colon-prefixed segments
        // (`/:mcp`, axum 0.7 syntax) panic in axum 0.8's `Router::route`;
        // rejecting them here yields `InvalidMcpPath` instead of a crash.
        if path.is_empty()
            || !path.starts_with('/')
            || path.contains("//")
            || path.contains('{')
            || path.contains('*')
            || path.split('/').any(|segment| segment.starts_with(':'))
        {
            return Err(RouterBuildError::InvalidMcpPath {
                value: rt.mount_path,
            });
        }
        // The MCP endpoint mounts GET+POST at `mount_path`. If a user, framework,
        // or OpenAPI route already owns that exact path, the later `merge` would
        // panic on overlapping method routes; surface it as a recoverable error
        // first (mirroring the OpenAPI collision preflight).
        reject_mcp_path_collisions(
            path,
            &route_list,
            &ctx.scoped_groups,
            config,
            ctx.openapi.as_ref(),
            &ctx.merge_routers,
            &ctx.nest_routers,
        )?;
        let docs = collect_openapi_docs(&route_list, &ctx.scoped_groups);
        // Pass the app's OpenAPI config (if any) so MCP tool `inputSchema`s
        // reuse the same registered component schemas as the served spec.
        let tools = crate::mcp::derive_tools(&docs, rt.expose_all, ctx.openapi.as_ref());
        Some((rt.mount_path, tools, rt.endpoint_layer))
    } else {
        None
    };

    // Build the per-route timeout override table before `route_list` and the
    // scoped groups are consumed by the mounting steps below.
    let route_timeouts = build_route_timeout_table(&route_list, &ctx.scoped_groups, config);

    let idempotency_layers = build_idempotency_layers(config, state)?;
    // Both `.layer(..)` custom layers and `.static_gate(..)` gate layers are
    // opaque app layers for idempotency: an auth/tenant layer in either slot
    // must force fail-closed replay so a cached mutation can't be served to a
    // different principal carrying the same Idempotency-Key.
    let opaque_app_layers_present = opaque_app_layers_override.unwrap_or_else(|| {
        custom_layers_require_fail_closed_idempotency(&ctx.custom_layers)
            || custom_layers_require_fail_closed_idempotency(&ctx.static_gate_layers)
    });
    // Capture the paths a user handler already owns BEFORE `route_list` is
    // consumed below, so the auto-mounted probes can yield to a user route at
    // the same path instead of panicking on an overlapping `GET` (issue #1971).
    let user_get_paths = collect_user_get_paths(&route_list, &ctx.scoped_groups);

    let mut router = mount_user_routes(
        route_list,
        &ctx.scoped_groups,
        &ctx.declared_routes,
        idempotency_layers.as_ref(),
        opaque_app_layers_present,
        state,
        config,
    )?;

    let dev_reload_enabled = dev::is_enabled_with_env(&crate::config::OsEnv);

    router = mount_framework_routes(router, config, dev_reload_enabled);

    let (mounted_probe_paths, router_with_probes) =
        mount_probe_endpoints(router, config, &user_get_paths);
    router = router_with_probes;

    router = mount_actuator_endpoints(router, config, &mounted_probe_paths)?;

    #[cfg(feature = "openapi")]
    if let Some(openapi_router) = openapi_router {
        router = router.merge(openapi_router);
    }

    // Static file serving. Fingerprinted assets (e.g. `autumn.a1b2c3d4.css`)
    // get `Cache-Control: public, max-age=31536000, immutable`; other files use
    // the default browser policy. With an embedded `static/` tree (feature
    // `embed-assets` plus a registered dir), serve `/static/*` from the binary.
    // Otherwise serve from the project's `static/` directory, which keeps
    // hot-reload working in dev.
    #[cfg(feature = "embed-assets")]
    let embedded_static = crate::assets::embedded_static_dir().is_some();
    #[cfg(not(feature = "embed-assets"))]
    let embedded_static = false;

    if embedded_static {
        #[cfg(feature = "embed-assets")]
        {
            router = router.route(
                "/static/{*path}",
                axum::routing::get(crate::assets::serve_embedded),
            );
        }
    } else {
        let env = crate::config::OsEnv;
        let static_dir = crate::app::project_dir("static", &env);
        router = router.nest_service("/static", tower_http::services::ServeDir::new(&static_dir));
    }
    router = router.layer(crate::assets::AssetCacheControlLayer);

    router = mount_scoped_groups(
        router,
        ctx.scoped_groups,
        idempotency_layers.as_ref(),
        state,
    );

    router = mount_raw_routers(
        router,
        ctx.merge_routers,
        ctx.nest_routers,
        idempotency_layers.as_ref(),
    );

    // Extract the pre-static gate layers (AppBuilder::static_gate) before
    // applying the rest of the middleware. They are applied LAST — after the MCP
    // dispatch clone is taken below — so a `tools/call` replay never traverses
    // the page-cache gate. In the SSG/ISG path the caller already drained these
    // into `try_build_router_with_static_inner`, so this take yields an empty
    // list there.
    let static_gate_layers = std::mem::take(&mut ctx.static_gate_layers);

    // Built once and shared (by clone — it wraps an `Arc` in-flight counter)
    // between the direct-route stack below and the late-mounted `/mcp`
    // envelope further down, so both ingress surfaces admit against the same
    // ceiling instead of each getting its own independent (never-shared)
    // counter. See `apply_middleware`'s `load_shed_layer` parameter doc.
    let load_shed_layer = build_load_shed_layer(config, state);
    #[cfg(feature = "mcp")]
    let mcp_load_shed_layer = load_shed_layer.clone();

    router = apply_middleware(
        router,
        config,
        state,
        ctx.exception_filters,
        ctx.custom_layers,
        #[cfg(feature = "maud")]
        ctx.error_page_renderer,
        ctx.session_store,
        route_timeouts,
        load_shed_layer,
        defer_security_headers,
    )?;

    if dev_reload_enabled {
        // One `Router::layer` call, not two: tuple order is OUTERMOST FIRST, so
        // `inject_live_reload` stays outer to `disable_static_cache` exactly as
        // the two chained `.layer()` calls used to leave it (issue #2193).
        router = router.layer((
            axum::middleware::from_fn(dev::inject_live_reload),
            axum::middleware::from_fn(dev::disable_static_cache),
        ));
    }

    // Dev request inspector: mount UI and apply recording middleware.
    // Only active when profile = "dev"; returns 404 for all other profiles.
    let is_dev_profile = matches!(config.profile.as_deref(), Some("dev" | "development"));
    if is_dev_profile {
        // Capture the matched route pattern for the dev error overlay.
        // Applied as a route_layer so MatchedPath is already set when this runs.
        router = router.route_layer(axum::middleware::from_fn(
            crate::middleware::dev::capture_matched_path_middleware,
        ));
    }
    if is_dev_profile {
        let buf = crate::inspector::InspectorBuffer::new(config.dev.inspector_capacity);
        let inspector_path = config.dev.inspector_path.clone();
        let threshold = config.dev.inspector_n_plus_one_threshold;

        // Mount the inspector UI routes.
        router = router.merge(crate::inspector::inspector_router(
            buf.clone(),
            &inspector_path,
        ));
        tracing::debug!(
            path = %inspector_path,
            "Mounted dev request inspector"
        );

        // Apply the recording middleware (outermost layer so it captures
        // all routes). Self-excludes inspector's own path prefix.
        let layer = crate::inspector::InspectorLayer::new(buf, threshold, inspector_path)
            .with_session_cookie_name(config.session.cookie_name.clone());
        router = router.layer(layer);
    }

    #[cfg(feature = "oauth2")]
    let http_interceptor = HttpInterceptorLayer::new(state.clone());
    #[cfg(not(feature = "oauth2"))]
    let http_interceptor = tower::layer::util::Identity::new();

    // Install the request's app as the ambient event-bus context, so a free
    // `events::publish` call in a handler or service dispatches against this app
    // and not the process-global bus. This keeps parallel in-process apps
    // (notably tests) isolated. One `layer` call for both: tuple order is
    // outermost first, so the event-bus context stays outer to the oauth2
    // interceptor, exactly as the two separate calls used to leave it (#2193).
    let router = router.layer((
        crate::events::EventAppContextLayer::new(state.clone()),
        http_interceptor,
    ));

    // Mount MCP last so its dispatch target — a clone of the fully-assembled
    // router with state applied — traverses the same routes, layers, and
    // middleware as an HTTP request. The clone is taken before the MCP route is
    // added, so `tools/call` never recurses into the MCP endpoint.
    //
    // `static_gate` is deliberately absent from the clone in both modes: the
    // gate layers are applied after the clone is taken. A page-cache gate only
    // redirects or rejects a browser, which is meaningless for a JSON-RPC
    // `tools/call`. MCP and API auth belong in route guards, `#[secured]`, or
    // the session, all of which do traverse the clone.
    //
    // Known limitation in static/ISR mode: with a `dist` manifest,
    // `try_build_router_with_static_inner` drains the global custom layers
    // (`AppBuilder::layer`) and applies them after this clone is taken, so a
    // `tools/call` replay skips them. The fully-dynamic path applies them
    // before the clone and keeps parity. A fix needs the appliers to be
    // re-usable; they are `FnOnce` today.
    #[cfg(feature = "mcp")]
    let router = if let Some((mount_path, tools, endpoint_layer)) = mcp_prepared {
        // The outermost `SecurityHeadersLayer` is applied after this clone, so
        // the dispatch snapshot would otherwise miss it. That layer also injects
        // `CspNonce` into request extensions, so a `tools/call` replay of a
        // handler using the `CspNonce` extractor would 500 when `csp_nonce` is
        // on. Re-attach it to the dispatch clone only: a direct request gets it
        // from the outer application, and `serve_mcp` discards the replay's
        // response headers, so no header is duplicated live. The gate stays off
        // the clone — a browser redirect is meaningless for JSON-RPC.
        let dispatch = router
            .clone()
            .layer(crate::security::SecurityHeadersLayer::from_config(
                &config.security.headers,
            ))
            .with_state(state.clone());
        // For header-based tenancy, forward the configured tenant header on
        // dispatch so tenant-scoped tools resolve the same tenant a direct HTTP
        // call would. Other sources key off already-forwarded headers/Host.
        let tenant_header = (config.tenancy.enabled && config.tenancy.source == "header")
            .then(|| config.tenancy.header_name.clone());
        let wiring = crate::mcp::McpWiring {
            // The CORS config drives the cross-origin Origin allowlist and the
            // endpoint's own OPTIONS preflight responses.
            cors: config.cors.clone(),
            // The same-origin shortcut is gated on the app's trusted-Host
            // policy so it can't be abused for DNS rebinding.
            trusted_hosts: TrustedHostPolicy::from_config(config),
            tenant_header,
            // Forward the configured CSRF header (default `x-csrf-token`) so
            // customized CsrfConfig::token_header deployments work via MCP.
            csrf_header: config.security.csrf.token_header.to_ascii_lowercase(),
            // The envelope is rate-limited below iff rate limiting is enabled;
            // when so, a tools/call is counted there and its replay is exempted
            // from the dispatch pipeline's limiter (avoiding double-counting).
            envelope_rate_limited: config.security.rate_limit.enabled,
            // `dispatch` above is cloned from `router`, which already carries
            // `load_shed_layer` (applied inside `apply_middleware`) — so when
            // the envelope below is ALSO wrapped with that same shared layer,
            // a tools/call must mark its replay exempt (avoiding double-
            // counting against the same in-flight counter).
            envelope_load_shed: mcp_load_shed_layer.is_some(),
            // The agent-authority audit path (#1691) writes through the app's
            // installed `AuditLogger` and mints its correlation id from the
            // injected entropy seam, both reached from state.
            state: state.clone(),
        };
        let mut mcp_router =
            crate::mcp::build_mcp_router(&mount_path, tools, dispatch, wiring, endpoint_layer);
        // NOTE: this envelope's inbound request-timeout layer is applied further
        // down, outer to the rate-limit layer (see
        // `apply_request_timeout_middleware` below). It must wrap the limiter so
        // a stalled Redis rate-limit decision is bounded by `request_timeout_ms`,
        // matching the main stack.
        // Gate the envelope under maintenance mode, mirroring the layer
        // `apply_middleware` installs for direct routes. The `/mcp` router merges
        // after that layer, so without this `initialize`/`tools/list` would keep
        // serving the tool catalog during maintenance; the `tools/call` replay is
        // already gated through the dispatch clone. Applied before
        // `TrustedProxiesLayer` so it is inner to it, letting the maintenance IP
        // allow-list read the proxy-resolved identity instead of a spoofable raw
        // `X-Forwarded-For`.
        mcp_router = mcp_router.layer(build_maintenance_layer(config, state));
        // Admission control / load shedding (#1006), mirroring the layer
        // `apply_middleware` installs for direct routes (see the comment
        // there). The `/mcp` router is merged after that layer, so without
        // this, `initialize`/`tools/list`/`tools/call` would bypass
        // `server.max_concurrent_requests` entirely. Reuses the SAME
        // `load_shed_layer` instance passed to `apply_middleware` above
        // (cloned, sharing its `Arc` in-flight counter) rather than building
        // a second, independently-counting layer — see that call site's
        // comment. `None` (the default) is a no-op, matching direct routes.
        if let Some(load_shed) = mcp_load_shed_layer {
            mcp_router = mcp_router.layer(load_shed);
        }
        // Stamp `ResolvedClientIdentity` on the *outer* `/mcp` request too. The
        // MCP route is merged after `apply_middleware`, so the centralized
        // `TrustedProxiesLayer` above does not wrap it; without this, the
        // endpoint's own DNS-rebinding / same-origin check would fall back to
        // the raw (possibly proxy-rewritten) `Host` and wrongly 403 a
        // same-origin browser client behind a TLS-terminating proxy. The
        // dispatch clone already carries its own copy of this layer.
        mcp_router = apply_trusted_proxies_middleware(mcp_router, config);
        // The MCP route is merged after the ingress upload guards
        // (`build_upload_layers`), so axum's
        // built-in 2 MiB `DefaultBodyLimit` — not the app's configured limit —
        // would otherwise govern the `tools/call` envelope's `Bytes` body. Apply
        // the same cap a direct JSON endpoint gets so larger-but-valid tool
        // payloads aren't rejected before dispatch.
        mcp_router = mcp_router.layer(axum::extract::DefaultBodyLimit::max(
            config.security.upload.max_request_size_bytes,
        ));
        // Rate-limit the envelope so `secure_mcp` auth rejections are throttled;
        // they never reach the dispatch clone's limiter, so credential guessing
        // would otherwise consume no per-client bucket. A successful tools/call
        // is counted once here and replayed with `RateLimitExempt`, so the
        // dispatch pipeline's limiter does not count it twice. No-op when rate
        // limiting is off, matching `envelope_rate_limited`.
        //
        // Known limitation with `key_strategy = AuthenticatedPrincipal` plus
        // session auth: the envelope keys on the IP fallback, because the session
        // layer that `populate_rate_limit_principal` reads runs inside
        // `apply_middleware` and does not wrap this late-merged router. The
        // tools/call replay is then exempt, so the dispatch clone's
        // principal-aware limiter is skipped too. A session-authenticated MCP
        // call therefore misses the per-user bucket a direct request would use.
        mcp_router = apply_rate_limit_middleware(mcp_router, config, state);
        // Bound the whole envelope by the global inbound deadline: the rate-limit
        // decision (a stalled Redis limiter would tie up `/mcp` indefinitely), the
        // metadata and auth work (initialize, tools/list, and `secure_mcp`
        // rejections that never reach the dispatch clone), and the in-process
        // `tools/call` dispatch. The `/mcp` router merges after `apply_middleware`,
        // so the timeout layer installed there does not wrap it. Applied outer to
        // the rate-limit layer above, matching the main stack, but inner to the
        // security-header and CORS layers below, so a timeout 503 still flows out
        // through them and stays CORS-readable. The mount path is fixed, so
        // route-level overrides cannot apply and an empty override table is passed.
        // The layer no-ops when the global timeout is disabled.
        //
        // Known limitation for tools/call: this timer wraps the whole POST,
        // including the dispatch replay, with the global default deadline. The
        // dispatch clone's own per-route timeout layer is inner to this one, so a
        // tool whose route declares `timeout = "off"` or a longer `timeout_ms` is
        // still capped at the global default over MCP. Honoring the per-route
        // policy would mean propagating the dispatched route's timeout out to this
        // single fixed-path endpoint, which has no per-route distinction at the
        // layer level. `mirror_cors = false`: the 503 already exits through this
        // router's outer `CorsLayer` from `apply_mcp_cors_layer`.
        mcp_router = apply_request_timeout_middleware(
            mcp_router,
            config,
            state.metrics.clone(),
            std::sync::Arc::new(std::collections::HashMap::new()),
            false,
        );
        // Security headers (HSTS/CSP/etc.), mirroring the `SecurityHeadersLayer`
        // `apply_middleware` installs for direct routes. The `/mcp` router merges
        // after that layer, so without this the envelope's `initialize`,
        // `tools/list`, auth 401/403, and rate-limit 429 responses would ship
        // without the configured `security.headers`. The `tools/call` replay's
        // headers are produced on the dispatch clone and discarded when
        // `serve_mcp` rebuilds the JSON-RPC response, so the envelope needs its own.
        mcp_router = mcp_router.layer(crate::security::SecurityHeadersLayer::from_config(
            &config.security.headers,
        ));
        // CORS grant outermost so every response — including auth 401/403, the
        // 413 body-limit rejection, and a 429 from the limiter above, all
        // produced before `serve_mcp` runs — is readable by an allowlisted
        // browser client instead of being masked as a CORS failure.
        mcp_router = crate::mcp::apply_mcp_cors_layer(mcp_router, &config.cors);
        router.merge(mcp_router)
    } else {
        router
    };

    // Apply the pre-static gate and the outermost `SecurityHeadersLayer` last,
    // after the MCP dispatch clone above. This keeps the gate out of the
    // `tools/call` path in fully-dynamic mode, matching the SSG/ISG path, while
    // still running it before session and the static cache for ordinary HTTP
    // requests. `SecurityHeadersLayer` goes outermost so a gate redirect or 401
    // still carries HSTS/CSP/nosniff; one application keeps CSP nonces consistent.
    //
    // In the SSG/ISG path `defer_security_headers` is true and
    // `try_build_router_with_static_inner` already drained the gate layers and
    // applied both outside the static-first middleware, so this block no-ops.
    let router = if defer_security_headers {
        router
    } else if static_gate_layers.is_empty() {
        router.layer(crate::security::SecurityHeadersLayer::from_config(
            &config.security.headers,
        ))
    } else {
        // ONE application for both: a `tower-layer` tuple puts its FIRST
        // element OUTERMOST, so `SecurityHeadersLayer` stays outside the gates
        // — the same order the two separate `.layer()` calls produced (the
        // gates were applied first, then security headers wrapped them). A
        // registered gate therefore costs the framework no extra nesting level
        // at all.
        tracing::debug!(
            count = static_gate_layers.len(),
            "Pre-static gate Tower layers applied"
        );
        router.layer((
            crate::security::SecurityHeadersLayer::from_config(&config.security.headers),
            ComposedRegisteredLayers::new(static_gate_layers),
        ))
    };

    Ok(router)
}

/// Parse `{name}` captures from a route path.
///
/// Mirrors the compile-time extractor in `autumn_macros::api_doc` so
/// runtime spec assembly (which sees scope prefixes that the macro
/// never does) produces consistent parameter lists.
#[cfg(feature = "openapi")]
pub fn extract_path_params(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut remaining = path;

    while let Some(start) = remaining.find('{') {
        let after_brace = &remaining[start + 1..];
        // `{{` is an escaped literal brace (matchit renders `{{`/`}}` as literal
        // `{`/`}`), not a parameter. Skip the escaped brace and continue,
        // mirroring `autumn_macros::api_doc::extract_path_params`. The prior
        // `rfind`-based variant dropped this branch and so injected a phantom
        // param for valid escaped-brace routes (`{{hello}}` -> `hello`).
        if let Some(rest) = after_brace.strip_prefix('{') {
            remaining = rest;
            continue;
        }
        let Some(end_rel) = after_brace.find('}') else {
            break;
        };

        let inner = &after_brace[..end_rel];
        // Isolate the parameter name from any `:constraint` suffix
        // (`{id:[0-9]+}` -> `id`).
        let name = inner.split(':').next().unwrap_or(inner).trim();
        // Brace-free guard: only emit a name that is non-empty and contains no
        // stray brace. On nested/unbalanced input the inner segment may still
        // hold a `{` (e.g. `"{a{b}"` -> inner `"a{b"`); dropping such names
        // keeps the emitted list brace-free, which the macro algorithm alone
        // would not (#1721).
        if !name.is_empty() && !name.contains('{') && !name.contains('}') {
            out.push(name.to_owned());
        }

        remaining = &after_brace[end_rel + 1..];
    }

    out
}

/// Handler that dynamically constructs the `OpenAPI` specification document per request
/// so deprecation and sunset statuses do not go stale.
#[cfg(feature = "openapi")]
async fn serve_openapi_spec(
    state: axum::extract::State<AppState>,
    axum::extract::Extension(config): axum::extract::Extension<
        std::sync::Arc<crate::openapi::OpenApiConfig>,
    >,
    axum::extract::Extension(docs): axum::extract::Extension<
        std::sync::Arc<Vec<crate::openapi::ApiDoc>>,
    >,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let refs: Vec<&crate::openapi::ApiDoc> = docs.iter().collect();
    let now = state.clock().now();
    let spec = crate::openapi::generate_spec_at(&config, &refs, now);
    let spec_json = serde_json::to_string_pretty(&spec)
        .unwrap_or_else(|e| format!("{{\"error\": \"failed to serialize spec: {e}\"}}"));
    (
        [(http::header::CONTENT_TYPE, "application/json")],
        spec_json,
    )
        .into_response()
}

/// Build an Axum sub-router that serves the generated `OpenAPI` document
/// and (optionally) a Swagger UI HTML page.
///
/// Returns `None` when `OpenAPI` generation is disabled, i.e. the user
/// never called [`AppBuilder::openapi`](crate::app::AppBuilder::openapi).
///
/// The spec is dynamically generated on request to prevent lifecycle status from going stale.
#[cfg(feature = "openapi")]
fn build_openapi_router(
    route_list: &[Route],
    scoped_groups: &[ScopedGroup],
    openapi_config: Option<&crate::openapi::OpenApiConfig>,
    session_cookie_name: &str,
    api_versions: &[crate::app::ApiVersion],
) -> Result<Option<axum::Router<AppState>>, RouterBuildError> {
    let Some(config) = openapi_config else {
        return Ok(None);
    };
    let mut config = config.clone();
    session_cookie_name.clone_into(&mut config.session_cookie_name);
    config.api_versions = api_versions.to_vec();

    // Validate user-provided paths up front so a typo like
    // `"openapi.json"` surfaces as a recoverable RouterBuildError
    // rather than an axum panic (`Paths must start with a '/'`).
    validate_route_path("openapi_json_path", &config.openapi_json_path)?;
    if let Some(path) = &config.swagger_ui_path {
        validate_route_path("swagger_ui_path", path)?;
        // Registering two GET handlers on the same path would cause an
        // axum `Route::route` panic, so reject collisions as a
        // configuration error instead.
        if path == &config.openapi_json_path {
            return Err(RouterBuildError::DuplicateOpenApiPath { path: path.clone() });
        }
    }

    let docs = collect_openapi_docs(route_list, scoped_groups);

    let json_path = config.openapi_json_path.clone();
    let swagger_path = config.swagger_ui_path.clone();
    let title = config.title.clone();

    let mut router = axum::Router::<AppState>::new()
        .route(&json_path, axum::routing::get(serve_openapi_spec))
        .layer(axum::extract::Extension(std::sync::Arc::new(
            config.clone(),
        )))
        .layer(axum::extract::Extension(std::sync::Arc::new(docs)));

    if let Some(path) = swagger_path {
        router = mount_swagger_ui_routes(router, &path, &title, &json_path);
    }

    tracing::debug!(
        openapi_json = %json_path,
        swagger_ui = ?config.swagger_ui_path,
        swagger_ui_version = crate::openapi::SWAGGER_UI_VERSION,
        "Mounted OpenAPI endpoints"
    );

    Ok(Some(router))
}

/// Join a nest/scope prefix with a child route path, matching
/// `axum::Router::nest` normalization.
///
/// `nest("/api", r)` mounts r's `/` at `/api` (not `/api/`), and any
/// other child path `/foo` at `/api/foo`. The collision check and the
/// path emitted into the `OpenAPI` spec must use the same shape or we
/// end up either missing real collisions (the reviewer's case:
/// `/api` + `/` recorded as `/api/` but axum routes it at `/api`) or
/// generating a spec whose URLs don't match what axum serves.
#[allow(dead_code)]
pub fn join_nested_path(prefix: &str, child: &str) -> String {
    if child == "/" || child.is_empty() {
        // axum mounts the root child at the prefix *verbatim*, keeping any
        // trailing slash: `nest("/api", route("/"))` is served at "/api" while
        // `nest("/api/", route("/"))` is served at "/api/" — and `MatchedPath`
        // reports the same string. Preserve the prefix as-is so the per-route
        // timeout table keys by exactly what the runtime looks up; only the
        // empty (root) prefix collapses to "/".
        if prefix.is_empty() {
            "/".to_owned()
        } else {
            prefix.to_owned()
        }
    } else {
        // Non-root children always join on a single slash, matching axum (e.g.
        // `nest("/api/", route("/users"))` resolves to "/api/users").
        let prefix_trimmed = prefix.trim_end_matches('/');
        if child.starts_with('/') {
            format!("{prefix_trimmed}{child}")
        } else {
            format!("{prefix_trimmed}/{child}")
        }
    }
}

/// Shared validator for user-supplied `OpenAPI` mount paths.
///
/// Catches the common typos that would otherwise manifest as axum
/// panics inside `Router::route` at startup:
///
/// * empty or missing leading slash,
/// * unbalanced `{` / `}` pairs,
/// * any `{…}` / `{*…}` capture or wildcard syntax (the mount points
///   are static endpoints — a user that needs templated paths shouldn't
///   be using this field), and
/// * any `*` wildcard character (axum treats these as catch-alls).
///
/// The check intentionally stays conservative: rejecting a few valid-
/// but-weird paths is far better than letting a typo like
/// `"openapi.json"` or `"/docs/{id}"` crash boot.
#[cfg(feature = "openapi")]
fn validate_route_path(field: &'static str, value: &str) -> Result<(), RouterBuildError> {
    let reject = |reason_fragment: &str| {
        Err(RouterBuildError::InvalidOpenApiPath {
            field,
            value: format!("{value:?} {reason_fragment}"),
        })
    };

    if value.is_empty() {
        return reject("(must be non-empty)");
    }
    if !value.starts_with('/') {
        return reject("(must start with '/')");
    }
    // Double-slash inside the path is almost always a typo (e.g.
    // `//v3/api-docs`) and axum normalizes it away on match, so
    // treating it as invalid avoids surprising "route can't be hit"
    // reports in the field.
    if value.contains("//") {
        return reject("(must not contain '//')");
    }

    let mut depth: i32 = 0;
    for ch in value.chars() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth < 0 {
                    return reject("(unbalanced '}')");
                }
            }
            '*' => return reject("(wildcard '*' is not allowed in an OpenAPI mount path)"),
            _ => {}
        }
    }
    if depth != 0 {
        return reject("(unbalanced '{')");
    }
    if value.contains('{') {
        return reject("(OpenAPI mount paths must be static; `{…}` captures are not allowed)");
    }
    Ok(())
}

/// Collect the exact `GET`/`WS` paths owned by the user's *typed* route table
/// (top-level routes plus scoped-group routes, after scope-prefix resolution).
///
/// Unlike [`collect_claimed_get_paths`], this deliberately excludes every
/// framework-mounted path: its sole purpose is to let the auto-mounted probe
/// endpoints ([`mount_probe_endpoints`]) detect when a *user* handler already
/// owns a probe path and yield to it, rather than panicking inside
/// `axum::Router::route` on an overlapping `GET` (issue #1971). A `WS` route is
/// a `GET` under the hood, so it claims the path too (mirroring
/// [`collect_claimed_get_paths`]). Opaque routers registered via
/// [`AppBuilder::merge`](crate::app::AppBuilder::merge) /
/// [`AppBuilder::nest`](crate::app::AppBuilder::nest) are not introspectable
/// and so are not covered here — the same limitation the OpenAPI/MCP collision
/// preflights carry.
fn collect_user_get_paths(
    route_list: &[Route],
    scoped_groups: &[ScopedGroup],
) -> std::collections::HashSet<String> {
    let mut owned: std::collections::HashSet<String> = std::collections::HashSet::new();
    for route in route_list {
        if route.method == http::Method::GET || route.method.as_str() == "WS" {
            owned.insert(route.path.to_owned());
        }
    }
    for group in scoped_groups {
        for route in &group.routes {
            if route.method == http::Method::GET || route.method.as_str() == "WS" {
                owned.insert(join_nested_path(&group.prefix, route.path));
            }
        }
    }
    owned
}

/// Path namespaces the framework owns wholesale, rather than as individual
/// routes.
///
/// `/static` is always mounted — as `nest_service` over `ServeDir`, or as
/// `GET /static/{*path}` under `embed-assets` — and `/_autumn` holds the
/// inspector, job status, mail previews and the unsubscribe endpoint.
///
/// These need namespace treatment rather than an exact-path claim because the
/// paths under them are not enumerable route-by-route: `ServeDir` serves
/// whatever is on disk. A plugin nested here is not only a startup panic (a
/// declared route AT the prefix, or a catch-all, makes `Router::nest` refuse
/// the overlap) — a declared SUB-path mounts cleanly and then **shadows** the
/// framework, so an artifact declaring `/static/app.js` would serve script
/// from the host's own origin. That is the outcome this lane's response
/// content-type allowlist exists to prevent, arriving by a different door.
const fn framework_namespaces() -> &'static [&'static str] {
    &["/static", "/_autumn"]
}

/// Whether `path` is inside `namespace` — the namespace itself, or anything
/// below it on a segment boundary. `/staticfoo` is NOT inside `/static`.
fn path_is_under_namespace(path: &str, namespace: &str) -> bool {
    path == namespace
        || path
            .strip_prefix(namespace)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Every path a framework-mounted `GET` handler owns: probes, actuator, htmx
/// assets, framework CSS, dev live-reload and inspector, mail previews and
/// unsubscribe, the story gallery, and the tracked-job status route.
///
/// Split out of [`collect_claimed_get_paths`] so the declared-plugin-route
/// preflight can use it without the `openapi` feature and without duplicating
/// this list — the two must not drift, or a path one knows about becomes a
/// startup panic the other never sees.
fn collect_framework_get_paths(config: &AutumnConfig) -> std::collections::HashSet<String> {
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Framework-mounted GETs. The probes are behind the same `health.enabled`
    // off-switch `mount_probe_endpoints` checks (#1971): claiming them while
    // nothing mounts them would refuse a plugin — or an OpenAPI/MCP mount path
    // — over a collision that cannot happen.
    if config.health.enabled {
        claimed.insert(config.health.path.clone());
        claimed.insert(config.health.live_path.clone());
        claimed.insert(config.health.ready_path.clone());
        claimed.insert(config.health.startup_path.clone());
    }
    // Some entries in `actuator_endpoint_paths` exist only to seed the runtime
    // startup-barrier allow-list (#1627) and are actually mounted with a
    // mutating method — `/webhooks/replay` is a POST. Claiming those as GETs
    // refuses a plugin's GET at a path where GET and the framework's POST
    // merge cleanly. `route_listing` already subtracts them for the same
    // reason; the two lists must agree or one refuses what the other permits.
    let actuator_mutating = crate::actuator::actuator_mutating_routes(
        &config.actuator.prefix,
        config.actuator.sensitive,
    );
    let mutating_paths: std::collections::HashSet<&str> = actuator_mutating
        .iter()
        .map(|(_, path)| path.as_str())
        .collect();
    for path in crate::actuator::actuator_endpoint_paths(
        &config.actuator.prefix,
        config.actuator.sensitive,
        config.actuator.prometheus,
    ) {
        if mutating_paths.contains(path.as_str()) {
            continue;
        }
        claimed.insert(path);
    }
    #[cfg(feature = "htmx")]
    {
        // Only claim the htmx path when the built-in handler is actually
        // mounted; when htmx is vendored via `autumn assets`, ServeDir serves
        // the file and the path must not appear in the claimed-routes set.
        if !crate::assets::htmx_is_vendored() {
            claimed.insert(crate::htmx::HTMX_JS_PATH.to_owned());
        }
        claimed.insert(crate::htmx::HTMX_CSRF_JS_PATH.to_owned());
        claimed.insert(crate::htmx::AUTUMN_WIDGETS_JS_PATH.to_owned());
        claimed.insert(crate::htmx::IDIOMORPH_JS_PATH.to_owned());
        claimed.insert(crate::htmx::HTMX_SSE_JS_PATH.to_owned());
    }
    // Framework CSS routes (flash/widget stylesheets) merge a GET
    // unconditionally whenever their feature is on, before the late-merged
    // OpenAPI/MCP routers — reserve them so a colliding configured path
    // surfaces the typed collision error instead of panicking in
    // `router.merge`.
    #[cfg(feature = "flash")]
    claimed.insert(crate::flash::FLASH_CSS_PATH.to_owned());
    #[cfg(feature = "maud")]
    claimed.insert(crate::ui::WIDGETS_CSS_PATH.to_owned());
    // Dev live-reload endpoints are only mounted when the env vars
    // that enable them are set, but reserving the paths regardless
    // makes the error message deterministic across dev/prod.
    if dev::is_enabled_with_env(&crate::config::OsEnv) {
        claimed.insert(dev::LIVE_RELOAD_PATH.to_owned());
        claimed.insert(dev::LIVE_RELOAD_SCRIPT_PATH.to_owned());
    }
    // The dev request inspector merges a GET at `config.dev.inspector_path`
    // (only under the dev profile), before the late-merged OpenAPI/MCP routers.
    // Reserve it so a mount path colliding with the inspector surfaces a
    // recoverable error instead of panicking in `router.merge`.
    if matches!(config.profile.as_deref(), Some("dev" | "development")) {
        // Both of them: the inspector mounts a detail template alongside the
        // index, and claiming only the configured path left
        // `{inspector_path}/requests/{id}` open whenever the inspector sits
        // outside the reserved `/_autumn` namespace.
        claimed.extend(crate::inspector::inspector_endpoint_paths(
            &config.dev.inspector_path,
        ));
    }
    #[cfg(feature = "mail")]
    if config
        .mail
        .preview_routes_enabled(config.profile.as_deref())
    {
        claimed.insert(crate::mail::MAIL_PREVIEW_PATH.to_owned());
        claimed.insert("/_autumn/mail/messages/{message_id}".to_owned());
        claimed.insert("/_autumn/mail/previews/{mailer}/{method}".to_owned());
    }
    // The widget story gallery merges GETs at `/_stories` and
    // `/_stories/{slug}` when `stories.enabled` resolves true, before the
    // late-merged OpenAPI/MCP routers — reserve them so a colliding
    // configured mount path surfaces the typed collision error instead of
    // panicking in `router.merge`.
    #[cfg(feature = "maud")]
    if config.stories.enabled {
        claimed.insert(crate::stories::STORIES_PATH.to_owned());
        claimed.insert("/_stories/{slug}".to_owned());
    }
    // The default unsubscribe endpoint merges a GET (+POST) at `UNSUBSCRIBE_PATH`
    // before the late-merged OpenAPI/MCP routers, so reserve it too — otherwise an
    // OpenAPI/MCP mount configured at `/_autumn/unsubscribe` passes this preflight
    // and then panics in `router.merge` instead of surfacing the typed collision.
    #[cfg(feature = "mail")]
    if config.mail.should_mount_unsubscribe_endpoint() {
        claimed.insert(crate::mail::UNSUBSCRIBE_PATH.to_owned());
    }
    // The tracked-job status endpoint merges a GET before the late-merged
    // OpenAPI/MCP routers, so reserve it too (same rationale as unsubscribe
    // above): an OpenAPI/MCP mount at this path should surface the typed
    // collision instead of panicking in `router.merge`.
    if config.jobs.tracking.route_enabled {
        claimed.insert(crate::job_tracking::JOB_STATUS_ROUTE_PATH.to_owned());
    }
    claimed
}

/// Gather every path that a `GET` (or `WS`, which mounts as a `GET`) handler
/// will already own by the time a late-merged sub-router (`OpenAPI` or MCP) is
/// added: user routes (top-level + scoped groups) plus framework-mounted `GET`s
/// (probes, actuator, htmx assets, dev live-reload, mail previews). Shared by
/// the `OpenAPI` and MCP mount-collision preflights so they stay in lockstep.
#[cfg(feature = "openapi")]
fn collect_claimed_get_paths(
    route_list: &[Route],
    scoped_groups: &[ScopedGroup],
    config: &AutumnConfig,
) -> std::collections::HashSet<String> {
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for route in route_list {
        if route.method == http::Method::GET || route.method.as_str() == "WS" {
            claimed.insert(route.path.to_owned());
        }
    }
    for group in scoped_groups {
        for route in &group.routes {
            if route.method == http::Method::GET || route.method.as_str() == "WS" {
                claimed.insert(join_nested_path(&group.prefix, route.path));
            }
        }
    }
    claimed.extend(collect_framework_get_paths(config));
    claimed
}

/// Reject an MCP mount path that overlaps with a route already owning that
/// path. The MCP endpoint mounts `GET`+`POST` at `mount_path`; merging it would
/// panic in axum if a `GET` (any user/framework route) or `POST` (a user route)
/// already lives there. We surface a recoverable
/// [`RouterBuildError::McpPathCollision`] instead, reusing the same claimed-GET
/// gathering as the `OpenAPI` preflight so framework routes (health/probe,
/// actuator, htmx, dev) are covered too — e.g. `mount_mcp(config.health.path)`.
/// The configured `OpenAPI` JSON/UI/asset paths (which merge as `GET`s before
/// the MCP router) are checked as well.
#[cfg(feature = "mcp")]
fn reject_mcp_path_collisions(
    mount_path: &str,
    route_list: &[Route],
    scoped_groups: &[ScopedGroup],
    config: &AutumnConfig,
    openapi: Option<&crate::openapi::OpenApiConfig>,
    merge_routers: &[axum::Router<AppState>],
    nest_routers: &[(String, axum::Router<AppState>)],
) -> Result<(), RouterBuildError> {
    let mut claimed_get = collect_claimed_get_paths(route_list, scoped_groups, config);
    // The OpenAPI JSON/Swagger-UI endpoints (and UI assets) merge as GETs
    // before the MCP router, so a mount path colliding with them would panic.
    if let Some(openapi) = openapi {
        claimed_get.insert(openapi.openapi_json_path.clone());
        if let Some(ui_path) = &openapi.swagger_ui_path {
            claimed_get.insert(ui_path.clone());
            claimed_get.extend(crate::openapi::swagger_ui_asset_paths(ui_path));
        }
    }
    if claimed_get.contains(mount_path) {
        return Err(RouterBuildError::McpPathCollision {
            path: mount_path.to_owned(),
            method: "GET".to_owned(),
        });
    }
    // POST handlers come from user routes (framework routes are GETs).
    let post_owns_path = route_list
        .iter()
        .any(|route| route.method == http::Method::POST && route.path == mount_path)
        || scoped_groups.iter().any(|group| {
            group.routes.iter().any(|route| {
                route.method == http::Method::POST
                    && join_nested_path(&group.prefix, route.path) == mount_path
            })
        });
    if post_owns_path {
        return Err(RouterBuildError::McpPathCollision {
            path: mount_path.to_owned(),
            method: "POST".to_owned(),
        });
    }
    // A nest prefix P owns every route under P (`/P/...`), and those raw routers
    // are mounted before the MCP router. A mount path equal to P or falling
    // under `P/` would be shadowed by (or panic against) the nested router, so
    // reject it up front — mirroring the OpenAPI nest-collision preflight. The
    // framework unconditionally nests the static-file service at `/static`, so
    // reserve that prefix too.
    let nest_prefixes = nest_routers
        .iter()
        .map(|(prefix, _)| prefix.as_str())
        .chain(std::iter::once("/static"));
    for prefix in nest_prefixes {
        let prefix_slash = format!("{prefix}/");
        if mount_path == prefix || mount_path.starts_with(&prefix_slash) {
            return Err(RouterBuildError::McpPathCollision {
                path: mount_path.to_owned(),
                method: "nested router".to_owned(),
            });
        }
    }
    // Raw merged routers are opaque — axum does not expose their route table —
    // so an overlapping handler there would still panic at merge time. Warn so
    // operators know the check can't cover this case (mirrors the OpenAPI one).
    if !merge_routers.is_empty() {
        tracing::warn!(
            mcp_mount_path = %mount_path,
            merged_routers = merge_routers.len(),
            "MCP mount collision check skipped for AppBuilder::merge routers: \
             axum does not expose their route table, so an overlapping handler \
             will still panic at startup. Choose an MCP mount path that doesn't \
             overlap with any merged router's handlers."
        );
    }
    Ok(())
}

/// Reject `OpenAPI` mount paths that overlap with an existing `GET`
/// handler.
///
/// `axum::Router::merge` panics when the merged routers have method
/// handlers on the same path (e.g. two `GET` handlers on
/// `/openapi.json`). We surface that as a recoverable
/// [`RouterBuildError::OpenApiPathCollision`] so misconfiguration
/// produces an actionable error instead of a crash on startup.
///
/// We check against:
/// * user routes (top-level + scoped groups) that will be mounted
///   before the `OpenAPI` sub-router merges in,
/// * framework `GET`s: probes, actuator, htmx assets, and dev
///   live-reload when enabled,
/// * nest prefixes from [`AppBuilder::nest`](crate::app::AppBuilder::nest)
///   when the `OpenAPI` path falls under one.
///
/// Raw routers passed to [`AppBuilder::merge`](crate::app::AppBuilder::merge)
/// cannot be introspected — axum does not expose their route table.
/// We emit a `tracing::warn!` so operators know the check is
/// incomplete in that case.
#[cfg(feature = "openapi")]
fn reject_openapi_path_collisions(
    openapi_config: Option<&crate::openapi::OpenApiConfig>,
    route_list: &[Route],
    scoped_groups: &[ScopedGroup],
    merge_routers: &[axum::Router<AppState>],
    nest_routers: &[(String, axum::Router<AppState>)],
    config: &AutumnConfig,
) -> Result<(), RouterBuildError> {
    let Some(openapi) = openapi_config else {
        return Ok(());
    };

    // Gather every path a GET (or WS, which mounts as GET) will already
    // own by the time we merge.
    let claimed = collect_claimed_get_paths(route_list, scoped_groups, config);

    check_openapi_path_against(
        "openapi_json_path",
        &openapi.openapi_json_path,
        &claimed,
        nest_routers,
    )?;
    if let Some(path) = &openapi.swagger_ui_path {
        check_openapi_path_against("swagger_ui_path", path, &claimed, nest_routers)?;
        let mut claimed_with_openapi = claimed;
        claimed_with_openapi.insert(openapi.openapi_json_path.clone());
        for asset_path in crate::openapi::swagger_ui_asset_paths(path) {
            check_openapi_path_against(
                "swagger_ui_path",
                &asset_path,
                &claimed_with_openapi,
                nest_routers,
            )?;
        }
    }

    // Raw merged routers are opaque — we can't inspect their route
    // tables through the axum API. Warn instead of failing so users
    // know the check doesn't cover this code path.
    if !merge_routers.is_empty() {
        tracing::warn!(
            openapi_json_path = %openapi.openapi_json_path,
            swagger_ui_path = ?openapi.swagger_ui_path,
            merged_routers = merge_routers.len(),
            "OpenAPI mount collision check skipped for AppBuilder::merge routers: \
             axum does not expose their route table, so overlapping GET handlers \
             will still panic at startup. Choose OpenAPI paths that don't overlap \
             with any merged router's handlers."
        );
    }

    Ok(())
}

/// Evaluate a single `OpenAPI` path against the claimed-path set plus
/// any nest prefixes. Returns an `OpenApiPathCollision` error on
/// collision.
#[cfg(feature = "openapi")]
fn check_openapi_path_against(
    field: &'static str,
    path: &str,
    claimed: &std::collections::HashSet<String>,
    nest_routers: &[(String, axum::Router<AppState>)],
) -> Result<(), RouterBuildError> {
    if claimed.contains(path) {
        return Err(RouterBuildError::OpenApiPathCollision {
            field,
            path: path.to_owned(),
        });
    }
    // A nest prefix P owns every route under P (`/P/...`), so any
    // OpenAPI path that equals P or starts with `P/` will either
    // panic on merge (exact match) or nest inside the user's router
    // (where axum routing semantics decide which handler wins).
    // Reject both cases so the spec endpoint can't silently vanish.
    for (prefix, _) in nest_routers {
        let prefix_slash = format!("{prefix}/");
        if path == prefix || path.starts_with(&prefix_slash) {
            return Err(RouterBuildError::OpenApiPathCollision {
                field,
                path: path.to_owned(),
            });
        }
    }
    Ok(())
}

/// The HTTP method axum actually mounts a handler under — the effective verb the
/// duplicate preflight and the request-timeout table must both key on so the two
/// never drift.
///
/// `#[ws]` records the synthetic `WS` method, but the macro builds its handler
/// with `axum::routing::get` and [`group_and_mount_routes`] merges it as a `GET`
/// `MethodRouter`. So a `#[ws("/p")]` and a `#[get("/p")]` are the SAME mount as
/// far as axum is concerned and would panic on merge. Every other method mounts
/// under itself.
fn effective_mount_method(method: &http::Method) -> http::Method {
    if method.as_str() == "WS" {
        http::Method::GET
    } else {
        method.clone()
    }
}

/// Probe whether two path templates conflict under matchit — the SAME engine
/// axum 0.8 routes through — by inserting both into a throwaway router. axum's
/// `Router::route` forwards each template to matchit verbatim (brace syntax:
/// `{param}` / `{*wild}`), so a matchit `Conflict` here is exactly the mount
/// panic `reject_duplicate_user_routes` is preventing. Used only on the error
/// path to name the specific prior template a conflicting insert collided with.
fn paths_conflict_under_matchit(existing: &str, incoming: &str) -> bool {
    let mut probe: matchit::Router<()> = matchit::Router::new();
    // If `existing` is itself malformed its insert fails; then it isn't in the
    // tree and can't be the conflict partner — return false so the caller keeps
    // scanning earlier templates.
    if probe.insert(existing, ()).is_err() {
        return false;
    }
    matches!(
        probe.insert(incoming, ()),
        Err(matchit::InsertError::Conflict { .. })
    )
}

/// Refuse two routers nested at the same prefix.
///
/// `mount_raw_routers` gives every nested router a fallback before mounting it,
/// and axum cannot merge two method routers that both have one — so the second
/// nest at a prefix the first already owns panics with "Cannot merge two
/// `MethodRouter`s that both have a fallback" while the router is built.
///
/// No route-level check can see this: two sandboxed plugins sharing a prefix
/// while declaring *disjoint* routes collide nowhere in the route table. The
/// collision is between the mounts, so a prefix belongs to whoever nests first.
fn reject_duplicate_nest_prefixes(
    nest_routers: &[(String, axum::Router<AppState>)],
    declared_routes: &[crate::route_listing::RouteInfo],
) -> Result<(), RouterBuildError> {
    let mut nested_at: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (prefix, _) in nest_routers {
        if nested_at.insert(prefix.as_str()) {
            continue;
        }
        // Declared routes are the only attribution available — a raw nested
        // router is anonymous — so name whoever we can, and say so when we
        // cannot.
        let mut owners: Vec<&str> = declared_routes
            .iter()
            .filter(|declared| declared.path.starts_with(prefix.as_str()))
            .map(|declared| declared.handler.as_str())
            .collect();
        owners.sort_unstable();
        owners.dedup();
        let owners = if owners.is_empty() {
            "two undeclared nested routers".to_owned()
        } else {
            owners.join(", ")
        };
        return Err(RouterBuildError::DuplicateNestPrefix {
            prefix: prefix.clone(),
            owners,
        });
    }
    Ok(())
}

/// Does a declared route clash with a framework mount, as axum would see it?
///
/// The two halves are not symmetric, and conflating them is what this check
/// kept getting wrong in both directions:
///
/// * **The same exact path** merges across methods. `GET /health` and a
///   declared `POST /health` land in one `MethodRouter` and coexist, so this is
///   a clash only when the methods are the same.
/// * **A different template at the same shape** — `/_stories/{slug}` against a
///   declared `/_stories/{id}` — is a *matchit* conflict, and matchit sits
///   above method routing: axum refuses the second template whatever method it
///   carries. Gating this on GET let a declared `POST /_stories/{id}` through
///   to a startup panic.
fn framework_route_clashes(
    framework_path: &str,
    framework_method: &str,
    declared_path: &str,
    declared_method: &str,
) -> bool {
    if framework_path == declared_path {
        return declared_method.eq_ignore_ascii_case(framework_method);
    }
    paths_conflict_under_matchit(framework_path, declared_path)
}

/// Fail-fast preflight for issue #1012: reject two user- or plugin-registered
/// routes that resolve to the same `(method, path)` before
/// [`group_and_mount_routes`] hands overlapping method routes to
/// [`axum::routing::MethodRouter::merge`] (which panics inside
/// `Router::route` at startup).
///
/// **Coverage** — mirrors `collect_route_infos`'s scope-prefix resolution so
/// duplicates across the same source, across sources (top-level +
/// scoped/plugin, plugin + plugin), and across `.scoped(...)` groups are
/// caught uniformly. `#[repository]`-generated API routes land in
/// `route_list` like any other route macro output, so they are covered
/// for free.
///
/// **Not covered — opaque routers**:
/// * [`AppBuilder::merge`](crate::app::AppBuilder::merge) — axum does not
///   expose the merged router's route table.
/// * [`AppBuilder::nest`](crate::app::AppBuilder::nest) — same limitation.
///
/// A non-empty opaque table emits a `tracing::warn!` (same pattern as the
/// existing `OpenAPI` and MCP merge-router warnings) so operators know the
/// preflight cannot see inside — an overlap involving one of those routers
/// will still surface as an axum startup panic.
///
/// The first pairwise collision wins: `existing` names the handler that
/// registered the path first (in the iteration order used by the actual
/// mount step), `incoming` names the duplicate that triggered the error.
fn reject_duplicate_user_routes(
    route_list: &[Route],
    scoped_groups: &[ScopedGroup],
    merge_routers: &[axum::Router<AppState>],
    nest_routers: &[(String, axum::Router<AppState>)],
    declared_routes: &[crate::route_listing::RouteInfo],
    config: &AutumnConfig,
) -> Result<(), RouterBuildError> {
    // `claimed` keys on `(effective_method, exact_path)`. The value is the
    // first-seen handler name, so an exact-duplicate error can name both sides
    // (AC #2). Iterate in mount order: top-level routes first
    // (`group_and_mount_routes`), then scoped groups (`mount_scoped_groups`).
    //
    // The key is the exact path string, not the normalized shape. axum merges the
    // same exact path across distinct methods (AC #4: `GET /admin` + `POST
    // /admin`), so a same-shape clash is a duplicate only when the exact path and
    // effective method both match. Cross-method shape conflicts are handled below.
    let mut claimed: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();

    // Method-independent path-shape conflicts are delegated to matchit, the same
    // engine axum 0.8 routes through, rather than a hand-rolled shape normalizer.
    // Every distinct exact template is inserted into a throwaway
    // `matchit::Router`. An `InsertError::Conflict` means the two templates overlap
    // and axum's `Router::route` would reject them with a mount panic before any
    // method merging (`/users/{id}` vs `/users/{slug}`, `/u/{id}` vs `/u/{*rest}`,
    // `/cmd/{tool}/{sub}` vs `/cmd/{*path}`, `/file.{ext}` vs `/file.{kind}`). This
    // converges every capture-name, escaped-brace, and catch-all edge case on
    // axum's own semantics. See `matchit_agrees_with_axum_route_conflicts` for the
    // parity guard that fails loudly if matchit ever drifts from axum.
    //
    // Exact-duplicate templates legitimately merge across distinct methods (AC
    // #4), so identical strings are deduplicated before insertion — re-inserting
    // one would falsely self-conflict. They fall through to the method-keyed
    // `claimed` check, which alone tells a real duplicate from a legal
    // cross-method registration.
    let mut shape_router: matchit::Router<String> = matchit::Router::new();
    // DISTINCT exact templates inserted into `shape_router`, in insertion order,
    // paired with their handler name. matchit's `InsertError::Conflict { with }`
    // reports the conflicting route as an unescaped/merged node string that need
    // not equal any template we registered, so we recover the conflict partner
    // ourselves by re-probing this list (first prior template that conflicts
    // under matchit wins, matching the "first-seen is `existing`" convention).
    let mut inserted_shapes: Vec<(String, String)> = Vec::new();

    let mut record =
        |method: &http::Method, path: String, name: &str| -> Result<(), RouterBuildError> {
            let effective_method = effective_mount_method(method).to_string();

            // Shape conflict (method-independent) via the matchit oracle. Skip
            // templates whose EXACT string was already inserted: an identical
            // string is a legal cross-method merge, not a shape conflict, and
            // re-inserting it would self-conflict.
            let already_inserted = inserted_shapes.iter().any(|(p, _)| p == &path);
            if !already_inserted {
                match shape_router.insert(&path, name.to_owned()) {
                    Ok(()) => inserted_shapes.push((path.clone(), name.to_owned())),
                    Err(matchit::InsertError::Conflict { .. }) => {
                        // Name the specific prior template this one collides with.
                        let (existing_path, existing_name) = inserted_shapes
                            .iter()
                            .find(|(prior, _)| paths_conflict_under_matchit(prior, &path))
                            .cloned()
                            // Defensive fallback (a full-tree conflict with no
                            // single pairwise partner is not expected for real
                            // route templates): attribute to the first insert.
                            .unwrap_or_else(|| inserted_shapes[0].clone());
                        return Err(RouterBuildError::ConflictingRouteShape {
                            existing: existing_name,
                            existing_path,
                            incoming: name.to_owned(),
                            incoming_path: path,
                        });
                    }
                    // Any other `InsertError` (malformed param/catch-all syntax)
                    // is a single-template validity problem, not a cross-route
                    // conflict; leave it to the existing path-validation seams
                    // and axum itself rather than mislabeling it a shape clash.
                    Err(_) => {}
                }
            }

            // Exact-duplicate check: same effective method AND same exact path
            // → axum's `MethodRouter::merge` would panic. Distinct methods on
            // the same exact path are legal (axum merges them) and fall through.
            let key = (effective_method.clone(), path.clone());
            if let Some(existing) = claimed.get(&key) {
                return Err(RouterBuildError::DuplicateUserRoute {
                    method: effective_method,
                    path,
                    existing: existing.clone(),
                    incoming: name.to_owned(),
                });
            }
            claimed.insert(key, name.to_owned());
            Ok(())
        };

    for route in route_list {
        record(&route.method, route.path.to_owned(), route.name)?;
    }
    for group in scoped_groups {
        for route in &group.routes {
            record(
                &route.method,
                join_nested_path(&group.prefix, route.path),
                route.name,
            )?;
        }
    }

    // A `nest` mount is opaque to axum's API, but a plugin that declared its
    // routes handed us the table anyway — and for a sandboxed plugin the manifest
    // is the mount, so that table is what `Router::nest` registers. Run those
    // declarations through the same oracle as the app's own routes. An artifact
    // declaring `GET /hello/greet` that the application already serves would
    // otherwise panic inside `Router::nest` ("Overlapping method route") and take
    // the process down at boot — containment failing open for the one input class
    // this lane exists to distrust. Shape clashes panic identically (`/hello/{id}`
    // against a nest declaring `/{slug}`, and `/hello/{*rest}` likewise), which is
    // why these go through `record` rather than an exact-path compare. Disjoint
    // paths under one prefix do not conflict, nor does a route at the prefix, so
    // this rejects only what axum would refuse.
    //
    // Declared routes are recorded last, so an application route always wins the
    // "first-seen is `existing`" convention and the error names the plugin as the
    // incoming side.
    for declared in declared_routes {
        // A method string that is not a valid HTTP token could not have been
        // mounted by axum either; leave it to the declaring seam rather than
        // inventing a collision for it here.
        let Ok(method) = http::Method::from_bytes(declared.method.as_bytes()) else {
            continue;
        };
        record(&method, declared.path.clone(), &declared.handler)?;
    }

    reject_declared_framework_collisions(declared_routes, config)?;

    // Raw merged / nested routers are opaque — axum does not expose their
    // route tables. Warn so operators know the check does not cover those
    // code paths (mirrors the OpenAPI and MCP merge-router warnings).
    if !merge_routers.is_empty() {
        tracing::warn!(
            merged_routers = merge_routers.len(),
            "duplicate-route preflight (#1012) skipped for AppBuilder::merge routers: \
             axum does not expose their route table, so an overlapping handler on a \
             method+path Autumn already owns will still panic at startup. Keep merged \
             routers on disjoint paths from your `.routes()`/`.scoped()` registrations."
        );
    }
    reject_duplicate_nest_prefixes(nest_routers, declared_routes)?;

    if !nest_routers.is_empty() {
        tracing::warn!(
            nested_routers = nest_routers.len(),
            declared_routes = declared_routes.len(),
            "duplicate-route preflight (#1012) covers AppBuilder::nest routers only \
             through declare_plugin_routes: axum does not expose a nested router's \
             route table, so any handler it serves that was NOT declared can still \
             panic at startup if it overlaps a method+path Autumn already owns. \
             Declared routes — a sandboxed plugin declares its whole manifest — ARE \
             checked; for anything else keep nested routers on disjoint prefixes \
             from your `.routes()`/`.scoped()` registrations."
        );
    }

    Ok(())
}

/// Refuse a declared plugin route that lands on something the framework
/// mounts.
///
/// Split out of [`reject_duplicate_user_routes`] because the framework's own
/// mounts are not in `route_list` — they are installed separately — so the
/// `record` oracle there cannot see them at all.
fn reject_declared_framework_collisions(
    declared_routes: &[crate::route_listing::RouteInfo],
    config: &AutumnConfig,
) -> Result<(), RouterBuildError> {
    // The framework's own GETs (probes, actuator, htmx assets, mail previews, …)
    // are mounted separately and are not in `route_list`, so the `record` oracle
    // in `reject_duplicate_user_routes` cannot see them: a manifest declaring
    // `GET /health` sailed past that check and still panicked at `Router::nest`.
    // Verified against axum 0.8.9 that only GET actually clashes there. A declared
    // HEAD or POST at a framework GET path merges cleanly into the same
    // `MethodRouter`, so refusing those would reject mounts axum accepts.
    //
    // Refuse rather than let the framework yield. A user route at a probe path
    // legitimately takes it over (#1971) — the developer owns their app. An
    // artifact the operator was told is sandboxed is a different principal:
    // silently handing it `/health`, which orchestrators read to decide whether
    // the process is alive, is worse than a loud refusal. Any path a user route
    // already owns is caught by the caller's `record` pass, so this fires only
    // where the framework really mounts.
    let framework_get_paths = collect_framework_get_paths(config);
    // The framework's mutating mounts, carrying their real methods. The GET
    // claim set deliberately excludes these paths, so without this pass a
    // declared PUT or POST at one of them is compared against nothing.
    let actuator_mutating_routes = crate::actuator::actuator_mutating_routes(
        &config.actuator.prefix,
        config.actuator.sensitive,
    );
    for declared in declared_routes {
        // Namespaces first, and for EVERY method: `/static` and `/_autumn` are
        // owned wholesale, so a declared route anywhere beneath them is refused
        // whether it would panic (at the prefix, or a catch-all) or mount
        // quietly and shadow what the framework serves there.
        if let Some(namespace) = framework_namespaces()
            .iter()
            .find(|namespace| path_is_under_namespace(&declared.path, namespace))
        {
            return Err(RouterBuildError::DuplicateUserRoute {
                method: declared.method.clone(),
                path: declared.path.clone(),
                existing: format!("autumn framework namespace {namespace}"),
                incoming: declared.handler.clone(),
            });
        }
        // A `WS` upgrade is a GET as far as axum's method router is concerned,
        // so it clashes wherever a declared GET would.
        let effective_method = if declared.method.eq_ignore_ascii_case("GET")
            || declared.method.eq_ignore_ascii_case("WS")
        {
            "GET"
        } else {
            declared.method.as_str()
        };
        // The framework mounts non-GET routes too, and only GET was ever
        // compared — so a manifest declaring `PUT {prefix}/loggers/{name}`
        // passed this check and panicked at the nest, because the actuator
        // mounts exactly that. These carry their real method, so the
        // comparison is method-aware rather than GET-shaped.
        for (framework_method, framework_path) in &actuator_mutating_routes {
            if framework_route_clashes(
                framework_path,
                framework_method,
                &declared.path,
                effective_method,
            ) {
                return Err(RouterBuildError::DuplicateUserRoute {
                    method: declared.method.clone(),
                    path: declared.path.clone(),
                    existing: format!("autumn framework route {framework_path}"),
                    incoming: declared.handler.clone(),
                });
            }
        }
        // Compare through matchit, not by string equality: a framework path
        // that carries a capture conflicts with a DIFFERENTLY-NAMED capture at
        // the same position, and axum refuses that shape regardless of method.
        // `/_stories/{slug}` is the live example — a manifest declaring
        // `/_stories/{id}` is an exact-string miss and a startup panic. The
        // configurable paths (probes, actuator prefix, dev inspector, job
        // status) can carry captures too, since an operator sets them.
        let collision = framework_get_paths.iter().find(|framework_path| {
            framework_route_clashes(framework_path, "GET", &declared.path, effective_method)
        });
        if let Some(framework_path) = collision {
            // `FrameworkRouteOverlap` would read better, but its `existing` and
            // `incoming` are `&'static str` and a plugin's handler name is
            // built at runtime; widening them is a breaking change to a public
            // enum. `DuplicateUserRoute` carries `String`s and still names the
            // method, the path and both sides, which is what an operator needs
            // to act.
            return Err(RouterBuildError::DuplicateUserRoute {
                method: declared.method.clone(),
                path: declared.path.clone(),
                // Name the framework template, not just the label: when the
                // clash is a shape conflict the two paths are not the same
                // string, and the operator needs to see what it collided with.
                existing: format!("autumn framework route {framework_path}"),
                incoming: declared.handler.clone(),
            });
        }
    }
    Ok(())
}

/// Attach a route's declared SEO defaults (#1182) as a request extension so the
/// [`SeoMeta`](crate::seo::SeoMeta) extractor can hand the handler a
/// pre-populated builder.
///
/// Attaching them here — rather than inside the route macro — keeps the layer
/// clear of the macro-ordering dance with the signature-rewriting guards
/// (`#[secured]`, `#[throttle]`, `#[step_up]`, `#[authorize]`), and means
/// static pre-rendering picks them up for free, since it drives this same
/// router.
///
/// Called from **both** mounting paths — [`group_and_mount_routes`] and
/// [`mount_scoped_groups`] — because a scoped route is just as entitled to its
/// declared defaults, and the extractor's infallibility means a miss would show
/// up as silently absent meta tags rather than an error.
///
/// Routes that never declared `seo(...)` skip the layer entirely and pay
/// nothing per request. The layer is applied to the route's own
/// `MethodRouter`, so sibling verbs mounted on the same path do not inherit it.
fn attach_seo_defaults(
    handler: axum::routing::MethodRouter<AppState>,
    seo: crate::seo::SeoRouteDefaults,
) -> axum::routing::MethodRouter<AppState> {
    if seo.is_empty() {
        return handler;
    }
    handler.layer(axum::Extension(seo))
}

fn group_and_mount_routes(
    route_list: Vec<Route>,
    idempotency_layers: Option<&BuiltIdempotencyLayers>,
    opaque_app_layers_present: bool,
    state: &AppState,
) -> axum::Router<AppState> {
    // Group routes by path so multiple methods on the same path
    // (e.g. GET /admin + POST /admin) are merged into a single
    // MethodRouter. Axum 0.7+ panics if .route() is called twice
    // with the same path — merging avoids this.
    let mut grouped: indexmap::IndexMap<&str, axum::routing::MethodRouter<AppState>> =
        indexmap::IndexMap::new();
    for route in &route_list {
        tracing::debug!(
            method = %route.method,
            path = route.path,
            name = route.name,
            "Mounted route"
        );
    }
    for route in route_list {
        let selected_layer = idempotency_layers
            .map(|layers| idempotency_layer_for_route(&route, layers, opaque_app_layers_present));
        let mut handler = route.handler;
        if let Some(layer) = selected_layer {
            handler = handler.layer(layer.clone());
        }
        handler = attach_seo_defaults(handler, route.seo);
        if let Some(version) = route.api_version {
            handler = handler.layer(axum::middleware::from_fn_with_state(
                state.clone(),
                api_versioning_middleware,
            ));
            handler = handler.layer(axum::Extension(RouteVersionMetadata {
                version: version.to_string(),
                sunset_opt_out: route.sunset_opt_out,
                secured: route.api_doc.secured,
                required_roles: route.api_doc.required_roles,
                has_policy: route.api_doc.has_policy,
            }));
        }
        grouped
            .entry(route.path)
            .and_modify(|existing| {
                *existing = std::mem::take(existing).merge(handler.clone());
            })
            .or_insert(handler);
    }

    let mut router = axum::Router::new();
    for (path, method_router) in grouped {
        router = router.route(path, method_router);
    }
    router
}

/// Mount the user route table, applying locale-prefixed routing (issue
/// #1251) when [`I18nConfig::locale_prefix_enabled`](crate::i18n::I18nConfig::locale_prefix_enabled)
/// is enabled. Delegates straight to [`group_and_mount_routes`] otherwise, so
/// apps that don't opt in see no behavior change.
#[cfg(feature = "i18n")]
fn mount_user_routes(
    route_list: Vec<Route>,
    scoped_groups: &[ScopedGroup],
    declared_routes: &[crate::route_listing::RouteInfo],
    idempotency_layers: Option<&BuiltIdempotencyLayers>,
    opaque_app_layers_present: bool,
    state: &AppState,
    config: &AutumnConfig,
) -> Result<axum::Router<AppState>, RouterBuildError> {
    if !config.i18n.locale_prefix_enabled {
        return Ok(group_and_mount_routes(
            route_list,
            idempotency_layers,
            opaque_app_layers_present,
            state,
        ));
    }

    let (included, excluded) = partition_routes_for_locale_prefix(
        route_list,
        &config.i18n.locale_prefix_exclude,
        &config.i18n.locale_prefix_exclude_exact,
    );

    let included_path_methods = route_list_path_methods(&included);
    let excluded_path_methods = route_list_path_methods(&excluded);
    let scoped_path_methods = scoped_group_path_methods(scoped_groups);
    let framework_path_methods = framework_probe_path_methods(config);
    let declared_path_methods = declared_path_methods(declared_routes);

    let valid_locales = validated_locale_prefix_locales(&config.i18n);

    if let Some(err) = detect_locale_prefix_path_collision(
        &included_path_methods,
        &excluded_path_methods,
        &scoped_path_methods,
        &framework_path_methods,
        &declared_path_methods,
        &valid_locales,
    ) {
        return Err(err);
    }

    let path_method_filters = path_method_filters(&included_path_methods);

    let content_router = group_and_mount_routes(
        included,
        idempotency_layers,
        opaque_app_layers_present,
        state,
    );
    let excluded_router = group_and_mount_routes(
        excluded,
        idempotency_layers,
        opaque_app_layers_present,
        state,
    );
    Ok(apply_locale_prefix_routing(
        excluded_router,
        &content_router,
        &path_method_filters,
        &config.i18n,
        &valid_locales,
    ))
}

// This variant never actually returns `Err` (only the `i18n`-enabled sibling
// above can, on a locale-prefix path collision) — but it must keep the same
// `Result` signature so the single call site doesn't need its own
// `#[cfg(feature = "i18n")]` branch.
#[cfg(not(feature = "i18n"))]
#[allow(clippy::unnecessary_wraps)]
fn mount_user_routes(
    route_list: Vec<Route>,
    _scoped_groups: &[ScopedGroup],
    _declared_routes: &[crate::route_listing::RouteInfo],
    idempotency_layers: Option<&BuiltIdempotencyLayers>,
    opaque_app_layers_present: bool,
    state: &AppState,
    _config: &AutumnConfig,
) -> Result<axum::Router<AppState>, RouterBuildError> {
    Ok(group_and_mount_routes(
        route_list,
        idempotency_layers,
        opaque_app_layers_present,
        state,
    ))
}

/// Splits `route_list` into `(included, excluded)` for locale-prefix
/// mounting: `excluded` routes either match a configured
/// [`I18nConfig::locale_prefix_exclude`](crate::i18n::I18nConfig::locale_prefix_exclude)
/// prefix (e.g. hand-written `/api/*` routes) or a literal
/// [`I18nConfig::locale_prefix_exclude_exact`](crate::i18n::I18nConfig::locale_prefix_exclude_exact)
/// path (auto-populated from `#[static_get]` routes), and mount unprefixed,
/// exactly as they would with locale-prefix routing off. `included` routes
/// get nested under every supported locale plus a bare-path redirect.
///
/// The two lists are matched differently on purpose: a prefix exclusion like
/// `/api` also swallows `/api/users`, which is the point for a hand-excluded
/// namespace, but the exact list must NOT do that — excluding a static route
/// like `/posts` must not also swallow an unrelated dynamic sibling route
/// like `/posts/{slug}` (Codex review).
#[cfg(feature = "i18n")]
fn partition_routes_for_locale_prefix(
    route_list: Vec<Route>,
    exclude_prefixes: &[String],
    exclude_exact: &[String],
) -> (Vec<Route>, Vec<Route>) {
    let mut included = Vec::new();
    let mut excluded = Vec::new();
    for route in route_list {
        if exclude_exact.iter().any(|p| p == route.path)
            || matches_locale_exclude_prefix(route.path, exclude_prefixes)
        {
            excluded.push(route);
        } else {
            included.push(route);
        }
    }
    (included, excluded)
}

/// `true` when `path` equals one of `prefixes` or starts with `{prefix}/`.
/// A trailing `/*` (or `/`) on a configured prefix is stripped before
/// comparing, so `"/api"` and `"/api/*"` are equivalent — except a bare `"/"`
/// (e.g. a `#[static_get("/")]` route added via
/// `exclude_static_routes_from_locale_prefix`), which is kept as-is and
/// matched exactly: stripping its trailing slash would normalize it to an
/// empty prefix, which the empty-prefix guard below then silently rejects,
/// so `"/"` would never actually get excluded (Codex review).
///
/// `pub` (the `router` module itself is `pub(crate)`, so this is already
/// crate-private) so `tenancy::strip_locale_prefix_for_tenancy` can reuse the
/// same exclusion semantics rather than a third divergent copy (`seo.rs`
/// keeps its own, feature-independent copy to avoid a hard dependency on
/// `i18n`-gated router internals; `tenancy.rs`'s caller is already
/// `i18n`-gated, so reuse here doesn't add one).
#[cfg(feature = "i18n")]
pub fn matches_locale_exclude_prefix(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|raw| {
        let prefix = raw.strip_suffix("/*").unwrap_or(raw.as_str());
        let prefix = if prefix == "/" {
            prefix
        } else {
            prefix.strip_suffix('/').unwrap_or(prefix)
        };
        !prefix.is_empty() && (path == prefix || path.starts_with(&format!("{prefix}/")))
    })
}

/// `true` when `locale` is safe to use as a literal [`Router::nest`](axum::Router::nest)
/// segment AND as a literal path segment in the generated redirect target
/// (`/{locale}{path}`): non-empty (an empty string would nest at `"/"`,
/// which axum panics on — nesting at the root isn't supported) and free of
/// characters axum's route syntax interprets specially — `/` (would
/// silently nest an extra sub-path instead of one opaque segment), `{`/`}`
/// (path-parameter capture syntax), `*` (wildcard capture), and a leading
/// `:` (axum 0.7 capture syntax — axum 0.8's `Router::route` panics on it
/// during assembly via `validate_v07_paths`, the same restriction
/// `InvalidMcpPath` above already guards against) — plus `?`/`#` and
/// whitespace, which axum accepts as a literal nest string but which a
/// client parses as a query/fragment delimiter, silently truncating the
/// redirect target (`/en?x/foo` parses as path `/en` + query `x/foo`)
/// (Codex review).
#[cfg(feature = "i18n")]
fn is_valid_locale_segment(locale: &str) -> bool {
    !locale.is_empty()
        && !locale.starts_with(':')
        && !locale.contains(['/', '{', '}', '*', '?', '#'])
        && !locale.chars().any(char::is_whitespace)
}

/// The subset of `i18n.supported_locales` that's actually valid and unique —
/// i.e. exactly the locales [`apply_locale_prefix_routing`] nests (invalid
/// entries are dropped by [`is_valid_locale_segment`], duplicates by
/// order-preserving dedup). Negotiation data (`LocaleRoutingConfig`, the
/// default-locale fallback) must be built from this validated list rather
/// than the raw config — otherwise a request could negotiate to a locale
/// that was silently skipped and has no nest, trading a config typo's
/// build-time no-op for a runtime 404 (Codex review).
#[cfg(feature = "i18n")]
fn validated_locale_prefix_locales(i18n: &crate::i18n::I18nConfig) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    i18n.supported_locales
        .iter()
        .filter(|locale| is_valid_locale_segment(locale) && seen.insert(locale.as_str()))
        .cloned()
        .collect()
}

/// Maps an HTTP method to the [`axum::routing::MethodFilter`] bit it
/// corresponds to. Falls back to `GET` for anything unrecognized — in
/// practice only the synthetic `WS` method, which callers must run through
/// [`effective_mount_method`] first (converting it to `GET`) anyway; this
/// catch-all just avoids ever silently dropping a method were that
/// invariant somehow violated.
#[cfg(feature = "i18n")]
fn method_filter_for(method: &http::Method) -> axum::routing::MethodFilter {
    use axum::routing::MethodFilter;
    match method.as_str() {
        "POST" => MethodFilter::POST,
        "PUT" => MethodFilter::PUT,
        "DELETE" => MethodFilter::DELETE,
        "PATCH" => MethodFilter::PATCH,
        "HEAD" => MethodFilter::HEAD,
        "OPTIONS" => MethodFilter::OPTIONS,
        "TRACE" => MethodFilter::TRACE,
        _ => MethodFilter::GET,
    }
}

/// The same map as [`route_list_path_methods`], for routes a plugin *declared*
/// rather than routes the application built.
///
/// The locale-prefix check compares generated paths against everything already
/// mounted, and a sandboxed plugin's manifest is a mount like any other — it
/// was simply not among the things being compared.
#[cfg(feature = "i18n")]
fn declared_path_methods(
    declared: &[crate::route_listing::RouteInfo],
) -> std::collections::HashMap<String, Vec<http::Method>> {
    let mut map: std::collections::HashMap<String, Vec<http::Method>> =
        std::collections::HashMap::new();
    for route in declared {
        let Ok(parsed) = http::Method::from_bytes(route.method.as_bytes()) else {
            // `WS` is not an HTTP method; it mounts as a GET.
            if route.method.eq_ignore_ascii_case("WS") {
                let methods = map.entry(route.path.clone()).or_default();
                if !methods.contains(&http::Method::GET) {
                    methods.push(http::Method::GET);
                }
                if !methods.contains(&http::Method::HEAD) {
                    methods.push(http::Method::HEAD);
                }
            }
            continue;
        };
        let effective = effective_mount_method(&parsed);
        let methods = map.entry(route.path.clone()).or_default();
        if !methods.contains(&effective) {
            methods.push(effective.clone());
        }
        if effective == http::Method::GET && !methods.contains(&http::Method::HEAD) {
            methods.push(http::Method::HEAD);
        }
    }
    map
}

/// For each DISTINCT path in `routes`, the effective HTTP methods actually
/// registered there — `WS`→`GET` via [`effective_mount_method`], and
/// `GET`→`+HEAD` (axum also serves `HEAD` through a `#[get]` handler),
/// mirroring exactly what [`build_route_timeout_table`] already does for
/// the same reason.
#[cfg(feature = "i18n")]
fn route_list_path_methods(
    routes: &[Route],
) -> std::collections::HashMap<String, Vec<http::Method>> {
    let mut map: std::collections::HashMap<String, Vec<http::Method>> =
        std::collections::HashMap::new();
    for route in routes {
        let effective = effective_mount_method(&route.method);
        let methods = map.entry(route.path.to_owned()).or_default();
        if !methods.contains(&effective) {
            methods.push(effective.clone());
        }
        if effective == http::Method::GET && !methods.contains(&http::Method::HEAD) {
            methods.push(http::Method::HEAD);
        }
    }
    map
}

/// The same per-path method collection as [`route_list_path_methods`], but
/// for routes mounted via `AppBuilder::scoped()` — resolved to their final
/// `{prefix}{path}` mount point via [`join_nested_path`] (the same helper
/// [`build_route_timeout_table`] uses), since a scoped group's routes are
/// never part of `route_list` and so are otherwise invisible to locale-prefix
/// collision detection even though they mount onto the SAME router
/// (`mount_scoped_groups`, after this module returns) and can collide with a
/// generated locale path just as easily as a top-level route (Codex review).
#[cfg(feature = "i18n")]
fn scoped_group_path_methods(
    scoped_groups: &[ScopedGroup],
) -> std::collections::HashMap<String, Vec<http::Method>> {
    let mut map: std::collections::HashMap<String, Vec<http::Method>> =
        std::collections::HashMap::new();
    for group in scoped_groups {
        for route in &group.routes {
            let resolved = join_nested_path(&group.prefix, route.path);
            let effective = effective_mount_method(&route.method);
            let methods = map.entry(resolved).or_default();
            if !methods.contains(&effective) {
                methods.push(effective.clone());
            }
            if effective == http::Method::GET && !methods.contains(&http::Method::HEAD) {
                methods.push(http::Method::HEAD);
            }
        }
    }
    map
}

/// Health/liveness/readiness/startup probe paths reserved by the framework
/// (`mount_probe_endpoints`, mounted well after this module returns) — all
/// `GET` (+`HEAD`), and only when `config.health.enabled` (matching
/// `mount_probe_endpoints`'s own off-switch: when probes are disabled, none
/// of these paths are reserved). Included in locale-prefix collision
/// detection so a probe path that happens to equal a generated locale path
/// is caught before axum panics on the later overlapping mount (Codex
/// review).
///
/// Actuator/OpenAPI/MCP mount paths are a similar, currently-undetected risk
/// (see the reply on this finding), but aren't included here: unlike the
/// probe paths, threading `OpenApiConfig`/the MCP mount path into this
/// function requires plumbing well beyond `AutumnConfig` alone.
#[cfg(feature = "i18n")]
fn framework_probe_path_methods(
    config: &AutumnConfig,
) -> std::collections::HashMap<String, Vec<http::Method>> {
    let mut map = std::collections::HashMap::new();
    if !config.health.enabled {
        return map;
    }
    for path in [
        &config.health.path,
        &config.health.live_path,
        &config.health.ready_path,
        &config.health.startup_path,
    ] {
        if path.is_empty() {
            continue;
        }
        map.entry(path.clone())
            .or_insert_with(|| vec![http::Method::GET, http::Method::HEAD]);
    }
    map
}

/// Combines a path's registered methods into the single [`axum::routing::MethodFilter`]
/// the bare-path redirect must claim there.
#[cfg(feature = "i18n")]
fn path_method_filter(methods: &[http::Method]) -> axum::routing::MethodFilter {
    methods
        .iter()
        .map(method_filter_for)
        .reduce(axum::routing::MethodFilter::or)
        .unwrap_or(axum::routing::MethodFilter::GET)
}

/// For each locale-prefix-included path, the [`axum::routing::MethodFilter`]
/// the bare-path redirect must claim there — so it claims precisely what the
/// nested content will serve.
///
/// Claiming every method via `axum::routing::any` (the prior behavior)
/// would otherwise reserve, say, GET at `/health` for the redirect even
/// when only `POST /health` is a real included route — colliding with the
/// framework's own auto-mounted `GET /health` probe, which mounts later
/// (`mount_probe_endpoints`, well after this function returns) and has no
/// visibility into methods the redirect already claimed (Codex review).
#[cfg(feature = "i18n")]
fn path_method_filters(
    included_path_methods: &std::collections::HashMap<String, Vec<http::Method>>,
) -> Vec<(String, axum::routing::MethodFilter)> {
    let mut result: Vec<_> = included_path_methods
        .iter()
        .map(|(path, methods)| (path.clone(), path_method_filter(methods)))
        .collect();
    result.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    result
}

/// Detects a route path that collides with a path locale-prefix nesting will
/// itself generate — e.g. an app defines both `/foo` and `/en/foo` while
/// `en` is a supported locale: the bare-path redirect (mounted at every
/// locale-prefix-eligible path, claiming only the methods actually
/// registered there — see [`path_method_filters`]) already owns some methods
/// at `/en/foo`, so nesting `/foo`'s content under `/en` collides if those
/// methods overlap, and axum panics on the overlapping route at
/// router-construction time. Checked against `included`, `excluded`
/// (mounted verbatim at the top level), `scoped` (mounted later via
/// `mount_scoped_groups`, but onto the same router), and `framework` (the
/// health/live/ready/startup probes, mounted later still via
/// `mount_probe_endpoints`) so any pre-existing route is caught regardless
/// of how it was registered (Codex review).
///
/// Two DIFFERENT kinds of collision are distinguished, matching
/// `reject_duplicate_user_routes`'s established model:
///   - An EXACT path match is only a real conflict if the methods overlap —
///     axum legally merges the SAME path template across DIFFERENT methods
///     (`GET /foo` generating `GET /en/foo` must coexist with an existing
///     `POST /en/foo`).
///   - A DIFFERENT template that [`paths_conflict_under_matchit`] flags is a
///     conflict regardless of method: two templates with different capture
///     *names* at the same position (e.g. `/en/users/{id}` generated from
///     `/users/{id}`, vs. an existing `/en/users/{slug}`) can never coexist,
///     because matchit's tree rejects the shape clash before axum even gets
///     to method-router merging.
#[cfg(feature = "i18n")]
fn detect_locale_prefix_path_collision(
    included: &std::collections::HashMap<String, Vec<http::Method>>,
    excluded: &std::collections::HashMap<String, Vec<http::Method>>,
    scoped: &std::collections::HashMap<String, Vec<http::Method>>,
    framework: &std::collections::HashMap<String, Vec<http::Method>>,
    declared: &std::collections::HashMap<String, Vec<http::Method>>,
    valid_locales: &[String],
) -> Option<RouterBuildError> {
    let all_other_paths: Vec<&String> = excluded
        .keys()
        .chain(scoped.keys())
        .chain(framework.keys())
        .chain(declared.keys())
        .chain(included.keys())
        .collect();

    for locale in valid_locales {
        for (path, methods) in included {
            let generated = if path == "/" {
                format!("/{locale}")
            } else {
                format!("/{locale}{path}")
            };

            let exact_conflict = excluded
                .get(&generated)
                .into_iter()
                .chain(scoped.get(&generated))
                .chain(framework.get(&generated))
                // A sandboxed plugin's declared mount is a mount: without it
                // here, a manifest claiming `/en/foo` sailed past this check and
                // axum panicked at boot on the locale clone of `/foo`.
                .chain(declared.get(&generated))
                .chain(included.get(&generated))
                .any(|other_methods| methods.iter().any(|m| other_methods.contains(m)));

            let shape_conflict = all_other_paths
                .iter()
                .any(|p| **p != generated && paths_conflict_under_matchit(p, &generated));

            if exact_conflict || shape_conflict {
                return Some(RouterBuildError::LocalePrefixPathCollision {
                    locale: locale.clone(),
                    path: path.clone(),
                    generated,
                });
            }
        }
    }
    None
}

/// Builds the locale-prefixed router: `excluded_router` (and a bare-path
/// redirect for every `path_method_filters` entry, claiming only the exact
/// methods registered there — see [`path_method_filters`](fn@path_method_filters))
/// mount at the top level; `content_router` is cloned and nested once per entry in
/// `valid_locales` (see [`validated_locale_prefix_locales`] — already
/// deduped and filtered to safe `Router::nest` segments).
///
/// Cloning `content_router` — a cheap, `Arc`-backed `axum::Router` — rather
/// than the underlying [`Route`] list is what lets every locale share one
/// router build: no route definition is duplicated by hand or in code.
///
/// When `valid_locales` is empty (a degenerate config — locale-prefix
/// routing on with nothing valid to prefix with), no nest is created and the
/// bare-path redirect is skipped too: redirecting to an unnested
/// `/{default_locale}/...` target that structurally can't exist would just
/// swap a direct 404 for a 308-then-404 round trip.
#[cfg(feature = "i18n")]
fn apply_locale_prefix_routing(
    excluded_router: axum::Router<AppState>,
    content_router: &axum::Router<AppState>,
    path_method_filters: &[(String, axum::routing::MethodFilter)],
    i18n: &crate::i18n::I18nConfig,
    valid_locales: &[String],
) -> axum::Router<AppState> {
    let mut router = excluded_router;

    if !path_method_filters.is_empty() && !valid_locales.is_empty() {
        let mut redirect_router = axum::Router::<AppState>::new();
        for (path, filter) in path_method_filters {
            redirect_router = redirect_router.route(
                path,
                axum::routing::on(*filter, locale_prefix_redirect_handler),
            );
        }
        router = router.merge(redirect_router);
    }

    // Content resolution (#1384) must see the URL prefix, which is visible only
    // inside this nest. An app-wide ambient-locale layer sits outside it and
    // would negotiate `/es/posts` from `Accept-Language` alone, so the nest gets
    // its own scope layer.
    //
    // It refines the app-wide scope rather than building a fresh one. That
    // layer's chain comes from the loaded `Bundle`, which an app may have built
    // from a different `I18nConfig` than the router's. Rebuilding the chain here
    // would shadow it, letting `/es/...` resolve `#[translatable]` content down
    // one chain while `Locale::t` on the same request walked another. The config
    // chain is the fallback for one shape only: locale-prefix routing without
    // `.i18n()`/`.i18n_auto()`, where no bundle exists.
    let prefix_chain = i18n.resolved_fallback_chain();
    for locale in valid_locales {
        let nested = content_router
            .clone()
            .fallback(crate::middleware::error_page_filter::fallback_404_handler)
            .layer(crate::i18n::LocalePrefixScopeLayer::new(
                locale.clone(),
                prefix_chain.clone(),
                &i18n.default_locale,
            ))
            .layer(axum::Extension(crate::i18n::UriPrefixedLocale(
                locale.clone(),
            )));
        router = router.nest(&format!("/{locale}"), nested);
    }

    // Negotiation data for the `Locale` extractor (#1251). Covers apps that set
    // `locale_prefix_enabled` without calling `.i18n()`/`.i18n_auto()`, so the
    // bare-path redirect — and any handler under an excluded prefix that takes a
    // `Locale` — negotiates against the configured
    // `supported_locales`/`default_locale` instead of an empty list and a
    // hard-coded `"en"`. This stays authoritative for negotiation even with a
    // `Bundle` installed, because the router's reachable locale segments come
    // from `I18nConfig`, not the bundle; see `Locale::from_request_parts`. A
    // `Bundle` stays authoritative for `t()`/`t_with()` lookups only.
    //
    // `default_locale` is used here only as the negotiation fallback, so it must
    // name a locale nested above. One absent from the validated set would
    // negotiate to a locale with no `/{locale}` nest, and the bare-path redirect
    // would 308 straight into a 404. Fall back to the first mounted locale.
    let effective_default_locale = if valid_locales.contains(&i18n.default_locale) {
        i18n.default_locale.clone()
    } else {
        valid_locales
            .first()
            .cloned()
            .unwrap_or_else(|| i18n.default_locale.clone())
    };
    router.layer(axum::Extension(crate::i18n::LocaleRoutingConfig {
        supported_locales: valid_locales.to_vec(),
        default_locale: effective_default_locale,
    }))
}

/// Fallback handler mounted at every locale-prefix-eligible bare path:
/// 308-redirects to the negotiated locale's prefixed path, preserving the
/// query string. Reuses the unmodified [`Locale`](crate::i18n::Locale)
/// extractor (no [`UriPrefixedLocale`](crate::i18n::UriPrefixedLocale) is set
/// this far outside any locale nest) so the redirect target matches exactly
/// what the request would have resolved to anyway.
///
/// **Known limitation** (Codex review): a mutating form `POSTing` directly to
/// its bare, unprefixed path with a `[SubmitTokenLayer](crate::security::SubmitTokenLayer)`-protected
/// `_submit_token` hits this 308 *before* reaching the real handler.
/// `SubmitTokenLayer` caches 3xx responses so a replayed submit returns the
/// first response verbatim — it has no way to distinguish this redirect
/// from a handler-issued one it's deliberately designed to cache, so it
/// records this 308 against the token. The browser then re-POSTs the same
/// body (with the same token) to the now-current, already-prefixed URL,
/// where the token replays the *cached 308* instead of reaching the
/// handler. Closing this fully would mean teaching `SubmitTokenLayer` to
/// recognize and skip caching this specific redirect — out of scope here.
/// Forms should POST to the current (already locale-prefixed) path — e.g.
/// via [`widgets::localized_path`](crate::widgets::localized_path) or a
/// relative `action` — rather than a hardcoded bare path, which this
/// framework's own `locale_switcher` already does for links.
#[cfg(feature = "i18n")]
async fn locale_prefix_redirect_handler(
    locale: crate::i18n::Locale,
    uri: axum::http::Uri,
) -> axum::response::Redirect {
    let path = uri.path();
    // Axum's `nest(prefix, router)` makes the *bare* `prefix` (no trailing
    // slash) match the inner router's own `"/"` route — `prefix/` 404s. So
    // the root path's redirect target is `/{locale}`, not `/{locale}/`,
    // unlike every other path, which is a plain concatenation.
    let mut target = if path == "/" {
        format!("/{}", locale.tag())
    } else {
        format!("/{}{path}", locale.tag())
    };
    if let Some(query) = uri.query() {
        target.push('?');
        target.push_str(query);
    }
    axum::response::Redirect::permanent(&target)
}

const fn idempotency_layer_for_route<'a>(
    route: &Route,
    layers: &'a BuiltIdempotencyLayers,
    opaque_app_layers_present: bool,
) -> &'a IdempotencyLayer {
    if opaque_app_layers_present {
        &layers.manual
    } else if route_uses_generated_replay_stop(route) {
        &layers.route
    } else {
        &layers.manual
    }
}

const fn route_uses_generated_replay_stop(route: &Route) -> bool {
    matches!(
        route.idempotency,
        crate::route::RouteIdempotency::ReplayThroughInner
    )
}

fn custom_layers_require_fail_closed_idempotency(
    custom_layers: &[crate::app::CustomLayerRegistration],
) -> bool {
    custom_layers
        .iter()
        .any(|registered| !is_idempotency_transparent_app_layer(registered))
}

fn is_idempotency_transparent_app_layer(registered: &crate::app::CustomLayerRegistration) -> bool {
    registered
        .type_name
        .starts_with("autumn_web::session::SessionLayer<")
        || registered
            .type_name
            .starts_with("autumn::session::SessionLayer<")
        || registered.type_id
            == std::any::TypeId::of::<crate::session::SessionLayer<crate::session::MemoryStore>>()
        || is_i18n_bundle_extension_layer(registered.type_id)
}

#[cfg(feature = "i18n")]
fn is_i18n_bundle_extension_layer(type_id: std::any::TypeId) -> bool {
    type_id == std::any::TypeId::of::<axum::Extension<Arc<crate::i18n::Bundle>>>()
}

#[cfg(not(feature = "i18n"))]
const fn is_i18n_bundle_extension_layer(_type_id: std::any::TypeId) -> bool {
    false
}

#[cfg_attr(not(feature = "mail"), allow(unused_variables))]
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
fn mount_framework_routes(
    mut router: axum::Router<AppState>,
    config: &AutumnConfig,
    dev_reload_enabled: bool,
) -> axum::Router<AppState> {
    #[cfg(not(feature = "mail"))]
    let _ = config;

    // Framework-provided routes
    #[cfg(feature = "htmx")]
    {
        // When htmx is vendored via `autumn assets add htmx@…`, skip the
        // built-in handler so ServeDir serves the correctly-pinned file.
        // Axum explicit routes beat `nest_service`, so without this guard the
        // embedded 2.0.4 bytes would shadow any updated vendored version.
        if crate::assets::htmx_is_vendored() {
            tracing::debug!(
                path = crate::htmx::HTMX_JS_PATH,
                "htmx vendored via `autumn assets`; built-in handler skipped, ServeDir serves it"
            );
        } else {
            router = router.route(crate::htmx::HTMX_JS_PATH, axum::routing::get(htmx_handler));
            tracing::debug!(
                method = "GET",
                path = crate::htmx::HTMX_JS_PATH,
                name = format!("htmx {}", crate::htmx::HTMX_VERSION),
                "Mounted route"
            );
        }
        router = router.route(
            crate::htmx::HTMX_CSRF_JS_PATH,
            axum::routing::get(htmx_csrf_handler),
        );
        router = router.route(
            crate::htmx::AUTUMN_WIDGETS_JS_PATH,
            axum::routing::get(autumn_widgets_handler),
        );
        router = router.route(
            crate::htmx::IDIOMORPH_JS_PATH,
            axum::routing::get(idiomorph_handler),
        );
        router = router.route(
            crate::htmx::HTMX_SSE_JS_PATH,
            axum::routing::get(htmx_sse_handler),
        );
        tracing::debug!(
            method = "GET",
            path = crate::htmx::HTMX_CSRF_JS_PATH,
            name = "htmx csrf helper",
            "Mounted route"
        );
        tracing::debug!(
            method = "GET",
            path = crate::htmx::AUTUMN_WIDGETS_JS_PATH,
            name = "autumn widget runtime",
            "Mounted route"
        );
        tracing::debug!(
            method = "GET",
            path = crate::htmx::IDIOMORPH_JS_PATH,
            name = "idiomorph DOM morphing",
            "Mounted route"
        );
        tracing::debug!(
            method = "GET",
            path = crate::htmx::HTMX_SSE_JS_PATH,
            name = "htmx SSE extension",
            "Mounted route"
        );
    }

    // Framework-provided flash-message stylesheet. Served as a same-origin
    // asset (rather than inline styles) so the `.flash` classes emitted by
    // `Flash::render` stay compatible with a strict `style-src 'self'` CSP.
    #[cfg(feature = "flash")]
    {
        router = router.route(
            crate::flash::FLASH_CSS_PATH,
            axum::routing::get(flash_css_handler),
        );
        tracing::debug!(
            method = "GET",
            path = crate::flash::FLASH_CSS_PATH,
            name = "autumn flash stylesheet",
            "Mounted route"
        );
    }

    // Framework-provided widget stylesheet (#1215). Backs every `autumn-*`
    // class emitted by form/widgets/wizard/pagination/storage/job-tracking so
    // widgets render styled without an app-authored copy — Tailwind or not.
    #[cfg(feature = "maud")]
    {
        router = router.route(
            crate::ui::WIDGETS_CSS_PATH,
            axum::routing::get(widgets_css_handler),
        );
        tracing::debug!(
            method = "GET",
            path = crate::ui::WIDGETS_CSS_PATH,
            name = "autumn widget stylesheet",
            "Mounted route"
        );
    }

    if dev_reload_enabled {
        router = router.route(
            dev::LIVE_RELOAD_PATH,
            axum::routing::get(dev::live_reload_state_handler),
        );
        router = router.route(
            dev::LIVE_RELOAD_SCRIPT_PATH,
            axum::routing::get(dev::live_reload_script_handler),
        );
        tracing::debug!(
            state_path = dev::LIVE_RELOAD_PATH,
            script_path = dev::LIVE_RELOAD_SCRIPT_PATH,
            "Mounted dev live reload endpoints"
        );
    }

    #[cfg(feature = "mail")]
    if config
        .mail
        .preview_routes_enabled(config.profile.as_deref())
    {
        router = router.merge(crate::mail::mail_preview_router(
            config.mail.file_dir.clone(),
        ));
        tracing::debug!(
            path = crate::mail::MAIL_PREVIEW_PATH,
            "Mounted dev mail preview endpoints"
        );
    }

    // Widget story gallery (#1526) — off by default, opt-in in ANY profile
    // via `[stories] enabled = true` (profile-layered). Handlers read the
    // StoryRegistry from the AppState extension installed by
    // `AppBuilder::with_story_gallery`.
    #[cfg(feature = "maud")]
    if config.stories.enabled {
        router = router.merge(crate::stories::story_router());
        tracing::debug!(
            path = crate::stories::STORIES_PATH,
            "Mounted story gallery endpoints"
        );
    }

    // RFC 8058 one-click unsubscribe endpoint — opt-in via
    // `mail.mount_unsubscribe_endpoint` / `AppBuilder::mount_unsubscribe_endpoint`
    // so JSON-only apps never get an HTML endpoint they didn't request.
    #[cfg(feature = "mail")]
    if config.mail.should_mount_unsubscribe_endpoint() {
        router = router.merge(crate::mail::unsubscribe_router());
        tracing::debug!(
            path = crate::mail::UNSUBSCRIBE_PATH,
            "Mounted default unsubscribe endpoint"
        );
    }

    // Tracked-job status endpoint (enqueue_tracked / #[job] JobContext) — on
    // by default; opt out via `jobs.tracking.route_enabled = false`.
    if config.jobs.tracking.route_enabled {
        router = router.merge(crate::job_tracking::status_router());
        tracing::debug!(
            path = crate::job_tracking::JOB_STATUS_ROUTE_PATH,
            "Mounted tracked-job status endpoint"
        );
    }

    router
}

fn mount_probe_endpoints<S>(
    mut router: axum::Router<S>,
    config: &AutumnConfig,
    user_get_paths: &std::collections::HashSet<String>,
) -> (std::collections::HashSet<String>, axum::Router<S>)
where
    S: Clone + Send + Sync + 'static,
    AppState: axum::extract::FromRef<S>,
{
    // Probe endpoints (auto-mounted). Each probe is a `GET`; when a user route
    // already owns that exact path, yield to the user handler instead of
    // handing axum a second `GET` for the same path — which panics at startup
    // with a raw "Overlapping method route" message that names none of the
    // user's code (issue #1971). A user who hand-writes `GET /health` clearly
    // wants their handler, so the built-in steps aside and logs the override.
    let mut mounted_probe_paths = std::collections::HashSet::new();

    // Global off-switch (issue #1971): when `health.enabled = false`, the
    // framework mounts none of the built-in probes (health/live/ready/startup),
    // leaving those paths free for an app to own — or to expose nothing. The
    // returned `mounted_probe_paths` set stays empty, so the actuator overlap
    // guard treats every probe path as unclaimed by the framework.
    if !config.health.enabled {
        tracing::info!("health.enabled = false; built-in probe endpoints are not auto-mounted");
        return (mounted_probe_paths, router);
    }

    let mut mount_probe = |mut router: axum::Router<S>,
                           path: &str,
                           label: &'static str,
                           handler: axum::routing::MethodRouter<S>|
     -> axum::Router<S> {
        if user_get_paths.contains(path) {
            tracing::info!(
                probe = label,
                path,
                "a user route already owns this path; the built-in probe was \
                 not auto-mounted (the user handler wins)"
            );
            // Still record the ceded path: `mount_actuator_endpoints` keys its
            // overlap guard off this set, and a configured probe path stays a
            // collision hazard for the actuator even when a user route (not the
            // built-in probe) owns it. Dropping it here would let an actuator at
            // prefix "/" merge its own `GET /health` onto the user's `GET
            // /health` and axum would panic during construction instead of
            // returning a checked `FrameworkRouteOverlap` (issue #1971 P2).
            mounted_probe_paths.insert(path.to_owned());
            return router;
        }
        if mounted_probe_paths.insert(path.to_owned()) {
            router = router.route(path, handler);
        }
        router
    };

    router = mount_probe(
        router,
        &config.health.live_path,
        "liveness",
        axum::routing::get(crate::probe::live_handler::<AppState>),
    );
    router = mount_probe(
        router,
        &config.health.ready_path,
        "readiness",
        axum::routing::get(crate::probe::ready_handler::<AppState>),
    );
    router = mount_probe(
        router,
        &config.health.startup_path,
        "startup",
        axum::routing::get(crate::probe::startup_handler::<AppState>),
    );
    router = mount_probe(
        router,
        &config.health.path,
        "health",
        axum::routing::get(crate::health::handler::<AppState>),
    );
    tracing::debug!(
        health = %config.health.path,
        live = %config.health.live_path,
        ready = %config.health.ready_path,
        startup = %config.health.startup_path,
        "Mounted probe endpoints"
    );

    (mounted_probe_paths, router)
}

fn mount_actuator_endpoints(
    mut router: axum::Router<AppState>,
    config: &AutumnConfig,
    mounted_probe_paths: &std::collections::HashSet<String>,
) -> Result<axum::Router<AppState>, RouterBuildError> {
    // Actuator endpoints
    let actuator_sensitive = config.actuator.sensitive;
    let actuator_prometheus = config.actuator.prometheus;
    let actuator_paths = crate::actuator::actuator_endpoint_paths(
        &config.actuator.prefix,
        actuator_sensitive,
        actuator_prometheus,
    );
    if let Some(path) = actuator_paths
        .iter()
        .find(|path| mounted_probe_paths.contains(path.as_str()))
    {
        return Err(RouterBuildError::FrameworkRouteOverlap {
            path: path.clone(),
            existing: "probe endpoint",
            incoming: "actuator endpoint",
        });
    }
    router = router.merge(crate::actuator::actuator_router_with_prefix(
        &config.actuator.prefix,
        actuator_sensitive,
        actuator_prometheus,
    ));
    tracing::debug!(
        sensitive = actuator_sensitive,
        prometheus = actuator_prometheus,
        prefix = %config.actuator.prefix,
        "Mounted actuator endpoints"
    );
    if !actuator_prometheus {
        tracing::info!(
            config_key = "actuator.prometheus",
            "Prometheus scrape endpoint is disabled; app metrics recorded through autumn_web::metrics are still collected, and still visible as JSON under the `app` key of the actuator's metrics endpoint — only the Prometheus scrape format is gated. Set actuator.prometheus = true to expose it"
        );
    }
    Ok(router)
}

fn mount_scoped_groups(
    mut router: axum::Router<AppState>,
    scoped_groups: Vec<ScopedGroup>,
    idempotency_layers: Option<&BuiltIdempotencyLayers>,
    state: &AppState,
) -> axum::Router<AppState> {
    // Mount scoped route groups (each with its own middleware layer).
    for group in scoped_groups {
        let mut sub_router = axum::Router::new();
        for route in group.routes {
            tracing::debug!(
                method = %route.method,
                path = route.path,
                name = route.name,
                scope = %group.prefix,
                "Mounted scoped route"
            );
            // Scoped groups are wrapped by an opaque user-provided layer after
            // the route handlers are built. The idempotency storage key cannot
            // know whether that layer authorizes, audits, or resolves tenant
            // state from non-whitelisted headers/extensions, so cached hits
            // fail closed instead of replaying through a generated stop inside
            // the scoped route.
            let selected_layer = idempotency_layers.map(|layers| &layers.manual);
            let seo = route.seo;
            let mut handler = route.handler;
            if let Some(layer) = selected_layer {
                handler = handler.layer(layer.clone());
            }
            handler = attach_seo_defaults(handler, seo);
            if let Some(version) = route.api_version {
                handler = handler.layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    api_versioning_middleware,
                ));
                handler = handler.layer(axum::Extension(RouteVersionMetadata {
                    version: version.to_string(),
                    sunset_opt_out: route.sunset_opt_out,
                    secured: route.api_doc.secured,
                    required_roles: route.api_doc.required_roles,
                    has_policy: route.api_doc.has_policy,
                }));
            }
            sub_router = sub_router.route(route.path, handler);
        }
        sub_router = (group.apply_layer)(sub_router);
        router = router.nest(&group.prefix, sub_router);
    }
    router
}

fn mount_raw_routers(
    mut router: axum::Router<AppState>,
    merge_routers: Vec<axum::Router<AppState>>,
    nest_routers: Vec<(String, axum::Router<AppState>)>,
    idempotency_layers: Option<&BuiltIdempotencyLayers>,
) -> axum::Router<AppState> {
    // Merge user-supplied raw Axum routers (escape hatch).
    // Merged after annotated routes so annotated routes take precedence.
    for raw_router in merge_routers {
        tracing::debug!("Merged raw Axum router");
        let raw_router = if let Some(layers) = idempotency_layers {
            raw_router.layer(layers.manual.clone())
        } else {
            raw_router
        };
        router = router.merge(raw_router);
    }

    // Nest user-supplied raw Axum routers under path prefixes.
    for (prefix, raw_router) in nest_routers {
        tracing::debug!(prefix = %prefix, "Nested raw Axum router");
        // We explicitly apply the fallback to the nested router before nesting,
        // so that unmatched routes within this prefix are protected by global middleware.
        let nested_router =
            raw_router.fallback(crate::middleware::error_page_filter::fallback_404_handler);
        let nested_router = if let Some(layers) = idempotency_layers {
            nested_router.layer(layers.manual.clone())
        } else {
            nested_router
        };
        router = router.nest(&prefix, nested_router);
    }
    router
}

/// Content-type prefixes `CompressionPredicate` skips beyond what
/// `tower_http`'s own `DefaultPredicate` already excludes (images, gRPC,
/// SSE, small bodies): binary media and already-compressed formats waste CPU
/// to (re)compress, inflate archive transfer size, or can confuse media
/// players.
const COMPRESSION_EXCLUDED_CONTENT_TYPES: &[&str] = &[
    // Binary media — already-encoded by codec, not compressible by gzip/br.
    "audio/",
    "video/",
    "application/octet-stream",
    // Compressed archive formats — re-compressing wastes CPU.
    "application/zip",
    "application/gzip",
    "application/x-gzip",
    "application/zstd",
    "application/x-bzip2",
    "application/x-bzip",
    "application/x-rar-compressed",
    "application/vnd.rar",
    "application/x-7z-compressed",
    // Pre-compressed web fonts — WOFF/WOFF2 embed their own compression, so
    // gzip/br only wastes CPU and can inflate them. Raw fonts (`font/ttf`,
    // `font/otf`) are NOT excluded: they are uncompressed SFNT data that
    // genuinely benefits from transfer compression.
    "font/woff",
    "font/woff2",
];

/// `compress_when` predicate shared by both compression call sites (this
/// function's SSG/ISG path and `apply_middleware`'s inline fully-dynamic
/// path, #2371).
///
/// A hand-rolled `Predicate` wrapping `DefaultPredicate`, rather than
/// `DefaultPredicate` extended with 13 chained
/// `.and(NotForContentType::const_new(...))` calls (the form this replaces):
/// each `NotForContentType` embeds a ~24-byte `Str` enum, so 13 of them
/// nested via `tower_http`'s `And<Lhs, Rhs>` inflated `CompressionLayer`'s
/// own static SIZE by several hundred bytes — harmless while that size only
/// affected `apply_compression_middleware`'s own local, but #2371 also made
/// this predicate a member of `apply_middleware`'s `option_layer` `Either`,
/// whose size is fixed by its larger (`Some`) branch even in the `None`
/// (compression-off, the default) case. That inflated the per-request byte
/// count `config_alloc_gate.rs` gates for every app, not just ones with
/// compression on — caught by CI, not by this repo's `middleware_stack_depth.rs`
/// probe, which only counts clone *events*, not their size (see that file's
/// own "What this gate is blind to" section). A `&'static [&'static str]`
/// slice checked in a loop costs 16 bytes regardless of how many entries it
/// lists.
#[derive(Clone)]
struct CompressionPredicate {
    default: tower_http::compression::predicate::DefaultPredicate,
}

impl tower_http::compression::predicate::Predicate for CompressionPredicate {
    fn should_compress<B>(&self, response: &http::Response<B>) -> bool
    where
        B: http_body::Body,
    {
        self.default.should_compress(response)
            && response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_none_or(|content_type| {
                    !COMPRESSION_EXCLUDED_CONTENT_TYPES
                        .iter()
                        .any(|excluded| content_type.starts_with(excluded))
                })
    }
}

fn compression_predicate() -> CompressionPredicate {
    CompressionPredicate {
        default: tower_http::compression::predicate::DefaultPredicate::new(),
    }
}

/// Apply response compression, when enabled.
///
/// Used only by the SSG/ISG static path (`try_build_router_with_static_inner`),
/// which keeps this as its own standalone `Router::layer` call. The
/// fully-dynamic path (`apply_middleware`) instead folds the equivalent
/// `(NormalizeBodyLayer, CompressionLayer)` pair directly into its single
/// merged tuple via `option_layer` (#2371) — see the `compression_layer`
/// local there for why the pairing is required: `CompressionLayer`'s service
/// changes the response BODY type, which `Route::layer` absorbs via
/// `IntoResponse` but `option_layer`'s `Either` cannot on its own — both of
/// its branches must share one `Response` type.
fn apply_compression_middleware<S>(
    mut router: axum::Router<S>,
    config: &AutumnConfig,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    if config.compression.enabled {
        router = router.layer(
            tower_http::compression::CompressionLayer::new().compress_when(compression_predicate()),
        );
        tracing::info!("Response compression enabled (gzip/brotli)");
    }
    router
}

/// Kept as a router-level wrapper for this module's unit tests; the main
/// ingress stack composes the layer directly (see `apply_middleware`).
#[cfg(test)]
fn apply_cors_middleware<S>(router: axum::Router<S>, config: &AutumnConfig) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    match build_ingress_cors_layer(config) {
        Some(layer) => router.layer(layer),
        None => router,
    }
}

/// Build the ingress CORS layer, or `None` when no origins are configured.
///
/// Split out of [`apply_cors_middleware`] so the layer can join the composed
/// ingress stack rather than costing its own nesting level (issue #2193).
fn build_ingress_cors_layer(config: &AutumnConfig) -> Option<tower_http::cors::CorsLayer> {
    // CORS middleware (only applied when allowed_origins is non-empty)
    if config.cors.allowed_origins.is_empty() {
        return None;
    }
    let cors = build_cors_layer(&config.cors);
    tracing::info!(
        origins = ?config.cors.allowed_origins,
        credentials = config.cors.allow_credentials,
        "CORS enabled"
    );
    Some(cors)
}

/// Kept as a router-level wrapper for this module's unit tests; the main
/// ingress stack composes the layer directly (see `apply_middleware`).
#[cfg(test)]
fn apply_csrf_middleware<S>(
    router: axum::Router<S>,
    config: &AutumnConfig,
    signing_keys: Option<std::sync::Arc<crate::security::config::ResolvedSigningKeys>>,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    match build_csrf_layer(config, signing_keys) {
        Some(layer) => router.layer(layer),
        None => router,
    }
}

/// Build the CSRF layer, or `None` when CSRF is disabled.
///
/// Split out of [`apply_csrf_middleware`] so the layer can join the composed
/// ingress stack rather than costing its own nesting level (issue #2193).
fn build_csrf_layer(
    config: &AutumnConfig,
    signing_keys: Option<std::sync::Arc<crate::security::config::ResolvedSigningKeys>>,
) -> Option<crate::security::CsrfLayer> {
    // CSRF middleware (only applied when enabled)
    if config.security.csrf.enabled {
        // The CSRF token scan reads only a bounded body prefix
        // (`security.csrf.token_scan_bytes`, 2 MiB default) and streams the rest
        // through, so the cap comes from CSRF config, not from
        // `upload.max_request_size_bytes` — that would force whole uploads into
        // memory and defeat the streaming upload path.
        //
        // Clamp the prefix to the global body limit so the CSRF layer never
        // buffers more than `upload.max_request_size_bytes`. Normally the small
        // `token_scan_bytes` prefix wins; the `min` keeps it at 2 MiB and never
        // raises it to the upload limit. Only an operator who lowers the global
        // limit below the prefix cap clamps the scan down, and then the whole
        // body is within that limit anyway, so an early `_csrf` token is still in
        // range and anything larger is rejected by `DefaultBodyLimit`.
        let effective_scan_bytes = config
            .security
            .csrf
            .token_scan_bytes
            .min(config.security.upload.max_request_size_bytes);
        let mut csrf_layer = crate::security::CsrfLayer::from_config(&config.security.csrf)
            .with_max_scan_bytes(effective_scan_bytes);
        if let Some(keys) = signing_keys {
            csrf_layer = csrf_layer.with_signing_keys(keys);
        }
        for endpoint in &config.security.webhooks.endpoints {
            csrf_layer = csrf_layer.with_exempt_path(&endpoint.path);
        }
        // RFC 8058 one-click unsubscribe POSTs arrive from mailbox providers
        // with no Autumn CSRF cookie/header; exempt the endpoint only when the
        // framework owns it (opt-in), so a custom override keeps its own CSRF.
        #[cfg(feature = "mail")]
        if config.mail.should_mount_unsubscribe_endpoint() {
            csrf_layer = csrf_layer.with_exempt_path(crate::mail::UNSUBSCRIBE_PATH);
        }
        tracing::info!("CSRF protection enabled");
        Some(csrf_layer)
    } else {
        None
    }
}

/// Apply the one-time submit-token guard (issue #1360).
///
/// Enabled by default. The layer is applied *inner* to the CSRF layer (it is
/// registered before `apply_csrf_middleware`, so on the request path CSRF is
/// validated first): a request bearing a valid `_csrf` but an already-consumed
/// `_submit_token` is still short-circuited by this guard. The store backend
/// mirrors [`build_idempotency_layers`]; the `redis` backend reuses the
/// `[idempotency.redis]` connection settings.
///
/// Kept as a router-level wrapper for this module's unit tests; the main
/// ingress stack composes the layer directly (see `apply_middleware`).
#[cfg(test)]
fn apply_submit_token_middleware<S>(
    router: axum::Router<S>,
    config: &AutumnConfig,
    is_production: bool,
) -> Result<axum::Router<S>, RouterBuildError>
where
    S: Clone + Send + Sync + 'static,
{
    Ok(match build_submit_token_layer(config, is_production)? {
        Some(layer) => router.layer(layer),
        None => router,
    })
}

/// Build the one-time submit-token layer, or `None` when it is disabled.
///
/// Split out of [`apply_submit_token_middleware`] so the layer can join the
/// composed ingress stack rather than costing its own nesting level
/// (issue #2193). The production memory-backend guard still surfaces as an
/// `Err`, before any layer is applied.
fn build_submit_token_layer(
    config: &AutumnConfig,
    is_production: bool,
) -> Result<Option<crate::security::SubmitTokenLayer>, RouterBuildError> {
    let cfg = &config.security.submit_token;
    if !cfg.enabled {
        return Ok(None);
    }

    // Production guard for the resolved consumed-token backend. Submit tokens are
    // on by default, so the backend can land on the per-process memory store in
    // production, which cannot deduplicate submits across replicas. Mirrors
    // `fail_fast_on_invalid_idempotency_config`: an explicit
    // `[security.submit_token].backend = "memory"` in prod fails fast, while an
    // inherited default only warns, so upgrading Autumn never means "prod won't
    // boot without Redis".
    match cfg.production_memory_guard(config.idempotency.backend, is_production) {
        crate::security::config::SubmitTokenMemoryGuard::Ok => {}
        crate::security::config::SubmitTokenMemoryGuard::WarnInherited => {
            tracing::warn!(
                "[security.submit_token].backend resolved to the in-memory store in production \
                 (inherited from [idempotency].backend, which is unset or memory). \
                 Single-replica deployments are fine, but multi-replica deployments need a shared \
                 backend: configure [idempotency] with backend = \"redis\" (or set \
                 [security.submit_token].backend = \"redis\") so consumed tokens are shared across \
                 replicas — otherwise a duplicate submit can slip through on a different replica."
            );
        }
        crate::security::config::SubmitTokenMemoryGuard::FailExplicit => {
            return Err(RouterBuildError::InvalidSubmitTokenBackend(
                "the in-memory submit-token backend is not safe for multi-replica production use. \
                 Set `[security.submit_token].backend = \"redis\"` in autumn.toml (it reuses the \
                 [idempotency.redis] connection settings), or remove the explicit `backend` \
                 override to inherit `[idempotency].backend`."
                    .to_owned(),
            ));
        }
    }

    let ttl = Duration::from_secs(cfg.ttl_secs);
    // Backend selection: an explicit `[security.submit_token].backend` wins;
    // otherwise inherit `[idempotency].backend` so a Redis-configured app shares
    // one consumed-token store across replicas by default (issue #1360), while a
    // dev app on the default memory idempotency backend keeps memory tokens.
    // `resolved_backend` is the single source of truth so this cannot drift from
    // `build_idempotency_layers`.
    let backend = cfg.resolved_backend(config.idempotency.backend);
    let store: std::sync::Arc<dyn IdempotencyStore> = match backend {
        crate::config::IdempotencyBackend::Memory => {
            std::sync::Arc::new(MemoryIdempotencyStore::new(ttl))
        }
        #[cfg(feature = "redis")]
        crate::config::IdempotencyBackend::Redis => {
            match crate::idempotency::RedisIdempotencyStore::from_config(&config.idempotency) {
                Ok(s) => std::sync::Arc::new(s),
                Err(e) => return Err(RouterBuildError::InvalidIdempotencyBackend(e)),
            }
        }
        #[cfg(not(feature = "redis"))]
        crate::config::IdempotencyBackend::Redis => {
            return Err(RouterBuildError::InvalidIdempotencyBackend(
                "submit_token backend 'redis' requires the autumn-web 'redis' feature \
                 flag; rebuild with --features redis or switch to backend = \"memory\""
                    .to_owned(),
            ));
        }
    };

    let mut layer = crate::security::SubmitTokenLayer::new(store, cfg)
        .with_max_scan_bytes(config.security.upload.max_request_size_bytes);
    for endpoint in &config.security.webhooks.endpoints {
        layer = layer.with_exempt_path(&endpoint.path);
    }
    #[cfg(feature = "mail")]
    if config.mail.should_mount_unsubscribe_endpoint() {
        layer = layer.with_exempt_path(crate::mail::UNSUBSCRIBE_PATH);
    }
    tracing::info!(
        backend = ?backend,
        inherited = cfg.backend.is_none(),
        ttl_secs = cfg.ttl_secs,
        "One-time submit-token protection enabled"
    );
    Ok(Some(layer))
}

/// Build the CAPTCHA/bot-protection layer, or `None` when it is disabled.
///
/// Called only by [`apply_middleware`], which places it in the composed ingress
/// stack rather than spending its own `Router::layer` call — and therefore its
/// own nesting level — on it (issue #2193). The former
/// `apply_bot_protection_middleware` router wrapper was removed with its last
/// caller.
fn build_bot_protection_layer(
    config: &AutumnConfig,
) -> Option<crate::security::captcha::BotProtectionLayer> {
    if config.bot_protection.enabled {
        // Use the dedicated captcha_exempt_paths list — NOT csrf.exempt_paths —
        // so that a route exempt from CSRF for non-cookie auth reasons does not
        // automatically bypass bot-protection as well.
        let mut exempt = config.security.captcha_exempt_paths.clone();
        for endpoint in &config.security.webhooks.endpoints {
            exempt.push(endpoint.path.clone());
        }
        // One-click unsubscribe POSTs carry no CAPTCHA token; exempt the
        // framework-owned endpoint when mounted.
        #[cfg(feature = "mail")]
        if config.mail.should_mount_unsubscribe_endpoint() {
            exempt.push(crate::mail::UNSUBSCRIBE_PATH.to_owned());
        }
        let layer =
            crate::security::captcha::BotProtectionLayer::from_config(&config.bot_protection)
                .with_max_scan_bytes(config.security.upload.max_request_size_bytes)
                .with_exempt_paths(exempt);
        tracing::info!(
            provider = ?config.bot_protection.provider,
            dev_bypass = config.bot_protection.dev_bypass,
            "Bot protection (CAPTCHA) enabled"
        );
        Some(layer)
    } else {
        None
    }
}

async fn populate_rate_limit_principal(
    axum::extract::State(state): axum::extract::State<AppState>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Populate `RateLimitPrincipal` from the verified session identity only.
    //
    // Deliberately no fallback to a raw `Authorization` header: this shim runs as
    // a global layer outer to route-scoped auth (`RequireApiToken`), so any
    // bearer token visible here is unverified and attacker-controlled. Keying the
    // limiter on it would let a caller rotate the token to mint unlimited buckets
    // or forge another user's principal to exhaust theirs. With no verified
    // principal, the limiter's `extract_key` falls back to IP keying, the safe
    // default. API-token routes that want per-principal limiting should place a
    // `RateLimitLayer` inner to `RequireApiToken`, which sets the verified
    // principal id (see `RequireApiTokenService::call`).
    if let Some(session) = req.extensions().get::<crate::session::Session>() {
        let auth_session_key = state.auth_session_key();
        if let Some(user_id) = session.get(auth_session_key).await {
            req.extensions_mut()
                .insert(crate::security::RateLimitPrincipal(user_id));
        }
    }
    next.run(req).await
}

/// Kept as a router-level wrapper for the `/mcp` envelope; the main ingress
/// stack composes the layer directly (see `apply_middleware`).
#[cfg(feature = "mcp")]
fn apply_trusted_proxies_middleware<S>(
    router: axum::Router<S>,
    config: &AutumnConfig,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(build_trusted_proxies_layer(config))
}

/// Build the trusted-proxy resolution layer.
///
/// **Unconditional** — the `if` below gates only the log line; the layer itself
/// is always installed, because `ResolvedClientIdentity` must be stamped for
/// every request whether or not any proxy ranges are configured. Split out of
/// [`apply_trusted_proxies_middleware`] so the layer can join the composed
/// ingress stack rather than costing its own nesting level (issue #2193).
fn build_trusted_proxies_layer(config: &AutumnConfig) -> crate::security::TrustedProxiesLayer {
    let tp = &config.security.trusted_proxies;
    if tp.trust_forwarded_headers || !tp.ranges.is_empty() || tp.trusted_hops.is_some() {
        tracing::info!(
            ranges = ?tp.ranges,
            trusted_hops = ?tp.trusted_hops,
            "Centralized trusted-proxy resolution enabled"
        );
    }
    crate::security::TrustedProxiesLayer::from_config(tp)
}

/// Kept as a router-level wrapper for the `/mcp` envelope and this module's
/// unit tests; the main ingress stack composes the layer directly (see
/// `apply_middleware`).
#[cfg(any(test, feature = "mcp"))]
fn apply_rate_limit_middleware(
    mut router: axum::Router<AppState>,
    config: &AutumnConfig,
    state: &AppState,
) -> axum::Router<AppState> {
    let (limiter, principal_keying) = build_rate_limit_layers(config, state);
    if let Some(limiter) = limiter {
        router = router.layer(limiter);
    }
    // Applied second, so it is OUTER to the limiter on ingress: the principal
    // must be populated before `extract_key` runs.
    if principal_keying {
        router = router.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            populate_rate_limit_principal,
        ));
    }
    router
}

/// Build the rate-limit layer, plus a flag for whether the
/// authenticated-principal keying shim is needed.
///
/// Returns `(None, false)` when rate limiting is disabled. Split out of
/// [`apply_rate_limit_middleware`] so both layers can join the composed ingress
/// stack rather than costing two more nesting levels (issue #2193). The shim is
/// returned as a flag rather than a layer because it is an
/// `axum::middleware::from_fn_with_state` closure whose type cannot be named
/// across a function boundary; the caller constructs it in place.
fn build_rate_limit_layers(
    config: &AutumnConfig,
    state: &AppState,
) -> (Option<crate::security::RateLimitLayer>, bool) {
    if config.security.rate_limit.enabled {
        let tp = &config.security.trusted_proxies;
        let rl = &config.security.rate_limit;
        let has_top_level_proxy_config =
            tp.trust_forwarded_headers || !tp.ranges.is_empty() || tp.trusted_hops.is_some();
        // Preserve explicit rate-limit proxy config (legacy fields). The shared
        // top-level resolver is only injected when the rate-limit section carries
        // no proxy config of its own, preventing dev defaults from silently
        // overriding an operator's explicit security.rate_limit.trusted_proxies.
        let has_rate_limit_proxy_config =
            rl.trust_forwarded_headers || !rl.trusted_proxies.is_empty();
        // The framework default limiter shares its bucket with the MCP `/mcp`
        // envelope limiter (both built here), so it honors `RateLimitEnvelopeCounted`
        // to avoid double-counting an already-charged `tools/call`. User-installed
        // limiters don't, so MCP replays still consume their per-route buckets.
        let mut layer = crate::security::RateLimitLayer::from_config(rl)
            .honoring_mcp_exempt()
            .with_clock(state.clock.clone());
        if has_top_level_proxy_config && !has_rate_limit_proxy_config {
            let resolver = crate::security::ProxyResolver::from_config(tp);
            layer = layer.with_proxy_resolver(resolver);
        }
        tracing::info!(
            rps = config.security.rate_limit.requests_per_second,
            burst = config.security.rate_limit.burst,
            "Rate limiting enabled"
        );
        let principal_keying = config.security.rate_limit.key_strategy
            == crate::security::KeyStrategy::AuthenticatedPrincipal;
        (Some(layer), principal_keying)
    } else {
        (None, false)
    }
}

/// Kept as a router-level wrapper for this module's unit tests; the main
/// ingress stack composes the layer directly (see `apply_middleware`).
#[cfg(test)]
fn apply_upload_middleware<S>(router: axum::Router<S>, config: &AutumnConfig) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let (body_limit, upload_config) = build_upload_layers(config);
    // NOTE: this REPRODUCES the relative order `apply_middleware`'s `inner_stack`
    // encodes (extension inserter OUTER to the body limit); it does not share it.
    // Keep the two in sync. Applied inner-first here, because consecutive
    // `Router::layer` calls put the LAST one outermost — the opposite of a tuple.
    router
        .layer(body_limit)
        .layer(axum::Extension(upload_config))
}

/// Resolve the two always-applied upload guards: the global body-size cap and
/// the [`UploadConfig`](crate::security::config::UploadConfig) the `Multipart`
/// extractor reads per-file limits from.
///
/// Split out of [`apply_upload_middleware`] so [`apply_middleware`] can place
/// both in the composed ingress stack (issue #2193).
fn build_upload_layers(
    config: &AutumnConfig,
) -> (
    axum::extract::DefaultBodyLimit,
    crate::security::config::UploadConfig,
) {
    let upload_config = config.security.upload.clone();
    let max_request_size = upload_config.max_request_size_bytes;
    tracing::info!(
        max_request_size_bytes = max_request_size,
        max_file_size_bytes = upload_config.max_file_size_bytes,
        allowed_mime_types = ?upload_config.allowed_mime_types,
        "Request body size limits enabled (applies to all content types)"
    );
    // The global cap covers JSON, form, raw bytes, and multipart; the Multipart
    // extractor further refines it per the UploadConfig extension. The config is
    // handed back as a plain value: callers install it with `axum::Extension`,
    // whose `AddExtension` service inserts it into request extensions directly —
    // the same effect as the `axum::middleware::from_fn` that used to do it, but
    // without that wrapper's per-request boxed future and boxed `Next`
    // (issue #2193).
    (
        axum::extract::DefaultBodyLimit::max(max_request_size),
        upload_config,
    )
}

/// Exact-match health/probe paths that must always bypass admission-style
/// gates (maintenance mode, the startup barrier, load shedding): the
/// compat health endpoint plus the `/live`, `/ready`, `/startup` lifecycle
/// probes and the actuator's own `/health` alias. Callers additionally
/// exempt the whole actuator prefix (`with_health_prefix`), since these
/// gates are keyed on exact paths, not prefixes.
fn probe_bypass_paths(config: &AutumnConfig) -> Vec<String> {
    vec![
        config.health.path.clone(),
        config.health.live_path.clone(),
        config.health.ready_path.clone(),
        config.health.startup_path.clone(),
        crate::actuator::actuator_route_path(&config.actuator.prefix, "/health"),
    ]
}

/// Build the [`MaintenanceLayer`](crate::middleware::maintenance::MaintenanceLayer)
/// from config + state, with the health/probe paths that always bypass the gate.
///
/// Shared by [`apply_middleware`] (direct routes) and the late-mounted `/mcp`
/// envelope so both return the documented `503` identically when maintenance
/// mode is active — the `/mcp` router is merged after `apply_middleware`, so
/// without an explicit layer its `initialize`/`tools/list` would keep serving
/// the catalog during maintenance.
fn build_maintenance_layer(
    config: &AutumnConfig,
    state: &AppState,
) -> crate::middleware::maintenance::MaintenanceLayer {
    let maintenance_state = state
        .extension::<crate::maintenance::MaintenanceState>()
        .map(|s| (*s).clone())
        .unwrap_or_default();
    crate::middleware::maintenance::MaintenanceLayer::new(maintenance_state)
        .with_health_prefix(config.actuator.prefix.clone())
        .with_probe_paths(probe_bypass_paths(config))
}

/// Build the admission-control ([`LoadShedLayer`](crate::middleware::LoadShedLayer))
/// layer from config, or `None` when `server.max_concurrent_requests` is unset
/// or `0` — the default, preserving today's unlimited behavior with effectively
/// zero overhead. In [`apply_middleware`] the `None` case goes through
/// `tower::util::option_layer`, contributing an `Either` branch that forwards
/// straight to the inner service: no allocation, no `Route` box, no nesting
/// level. The `/mcp` envelope still installs nothing at all.
///
/// Reuses the same probe/actuator bypass list as [`build_maintenance_layer`]
/// so health/liveness/readiness probes are never shed under load (#1006).
fn build_load_shed_layer(
    config: &AutumnConfig,
    state: &AppState,
) -> Option<crate::middleware::LoadShedLayer> {
    // The ceiling is either hand-set or sourced from the committed capacity
    // contract (#1733). `resolve_admission_limit` owns that precedence — and
    // owns failing *open* on every contract problem, so a stale or missing
    // lockfile can never shed every request on the way up.
    let resolved = crate::capacity::resolve_configured_admission_limit(
        config.server.max_concurrent_requests,
        config.server.capacity_contract.as_deref(),
    );
    let limit = resolved.limit()?;
    if matches!(resolved, crate::capacity::AdmissionLimit::Contract(_)) {
        tracing::info!(
            limit,
            source = resolved.source(),
            contract = config
                .server
                .capacity_contract
                .as_deref()
                .unwrap_or_default(),
            "admission control sourced from the committed capacity contract"
        );
    }
    // Mirror CORS headers onto a shed 503 the same way the timeout middleware
    // does for the main stack (`mirror_cors = true` there): this layer sits
    // outside `CorsLayer` on direct routes, so without mirroring a
    // cross-origin browser client sees an opaque CORS failure instead of a
    // readable 503. Harmless (but redundant) at the `/mcp` mount point, since
    // that shares this same layer instance yet sits *inside* its own
    // `CorsLayer`, which overwrites these headers with its own regardless.
    let cors =
        (!config.cors.allowed_origins.is_empty()).then(|| std::sync::Arc::new(config.cors.clone()));
    Some(
        crate::middleware::LoadShedLayer::new(limit, state.metrics.clone())
            .with_health_prefix(config.actuator.prefix.clone())
            .with_probe_paths(probe_bypass_paths(config))
            .with_cors(cors),
    )
}

/// Build the shadow-mirroring layer, or `None` when `[shadow]` is off.
///
/// Also installs the [`ShadowHandle`](crate::shadow::ShadowHandle) into the
/// app state's runtime extension map, which is what
/// `{actuator-prefix}/shadow` reads. A replica with mirroring off installs
/// nothing and that endpoint reports a disabled mirror.
///
/// The clock and entropy source are taken from the app state rather than read
/// ambiently, so a [`#[sim_test]`](crate::sim_test) controls both the sampling
/// decision and the recorded timestamps.
fn build_shadow_layer(
    config: &AutumnConfig,
    state: &AppState,
) -> Option<crate::shadow::ShadowMirrorLayer> {
    if !config.shadow.is_active() {
        return None;
    }
    let target_base = config.shadow.target_base()?.to_owned();

    // The real transport needs an HTTP client. Without the `http-client`
    // feature there is nothing to dial the candidate with, so say so once and
    // leave the ingress stack untouched rather than pretending to mirror.
    #[cfg(not(feature = "http-client"))]
    {
        tracing::warn!(
            "[shadow] enabled but this build has no `http-client` feature; \
             traffic mirroring is inactive"
        );
        // Install an inactive handle anyway, so the actuator can say "this
        // replica has a target configured but cannot mirror" rather than
        // reporting the same thing as a replica that never configured
        // `[shadow]` at all.
        state.insert_extension(crate::shadow::ShadowHandle::inactive(target_base));
        return None;
    }

    #[cfg(feature = "http-client")]
    {
        let timeout = std::time::Duration::from_millis(config.shadow.timeout_ms);
        let transport = match crate::shadow::transport::HttpShadowTransport::new(
            timeout,
            config.shadow.max_body_bytes,
        ) {
            Ok(transport) => std::sync::Arc::new(transport),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "[shadow] could not build the mirroring HTTP client; traffic mirroring \
                     is inactive"
                );
                return None;
            }
        };

        let registry = crate::shadow::ShadowRegistry::new(config.shadow.max_records);
        state.insert_extension(crate::shadow::ShadowHandle {
            registry: registry.clone(),
            enabled: true,
            target: Some(target_base.clone()),
        });

        // One `[log] filter_parameters` list governs the access log, error
        // pages, failure capsules, and now the recorded divergence samples.
        let mut filter_parameters = config.log.filter_parameters.clone();
        filter_parameters.extend(crate::encryption::registered_encrypted_column_names());
        let filter = Arc::new(crate::log::filter::ParameterFilter::new(
            &filter_parameters,
            &config.log.unfilter_parameters,
        ));

        // Exempt the actuator's actual mounted paths as well as its prefix. With
        // `[actuator] prefix = "/"` the endpoints mount at the root, where no
        // prefix test can tell them from application routes; mirroring an
        // operator's `/metrics` poll would add candidate load and permanent false
        // divergences from a per-replica payload. `actuator_endpoint_paths` is the
        // same source the startup barrier seeds its allow-list from, so this
        // cannot drift from what is mounted.
        let mut exempt_paths = probe_bypass_paths(config);
        exempt_paths.extend(crate::actuator::actuator_endpoint_paths(
            &config.actuator.prefix,
            config.actuator.sensitive,
            config.actuator.prometheus,
        ));

        let selector = crate::shadow::MirrorSelector::new(
            config.shadow.sample_rate,
            &config.shadow.routes,
            &config.actuator.prefix,
            &exempt_paths,
        );

        tracing::info!(
            target = %target_base,
            sample_rate = config.shadow.sample_rate,
            routes = ?config.shadow.routes,
            "Shadow traffic mirroring enabled (GET/HEAD only)"
        );

        Some(crate::shadow::ShadowMirrorLayer::new(
            crate::shadow::MirrorSettings {
                target_base,
                timeout,
                max_in_flight: config.shadow.max_in_flight,
                max_body_bytes: config.shadow.max_body_bytes,
                max_sample_bytes: config.shadow.max_sample_bytes,
            },
            selector,
            registry,
            transport,
            filter,
            state.entropy_arc(),
            state.clock_arc(),
        ))
    }
}

/// Per-route timeout lookup table, keyed by the fully-qualified route template
/// (matching [`axum::extract::MatchedPath`]) and then by HTTP method, so an
/// override on one handler never bleeds onto sibling methods sharing the path
/// (e.g. `GET /items` vs `POST /items`). The nested layout also lets the
/// middleware resolve the deadline from a borrowed `&str` + `&Method`, avoiding
/// any allocation on exempt/disabled routes. Built once at router-assembly time
/// from each [`Route`]'s `timeout` field and shared (cheaply cloned) into the
/// global timeout middleware.
type RouteTimeoutTable = std::sync::Arc<
    std::collections::HashMap<
        String,
        std::collections::HashMap<http::Method, crate::route::RouteTimeout>,
    >,
>;

/// Error surfaced as the cause of the `503` when an inbound request exceeds its
/// wall-clock deadline. Carried into [`crate::error::AutumnError::service_unavailable`]
/// so the response flows through the standard Problem Details / error-page stack
/// (JSON for API clients, HTML for browsers) instead of a raw tower `BoxError`.
#[derive(Debug)]
struct RequestDeadlineExceeded {
    timeout_ms: u64,
}

impl std::fmt::Display for RequestDeadlineExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the server did not produce a response within the configured {}ms deadline",
            self.timeout_ms
        )
    }
}

impl std::error::Error for RequestDeadlineExceeded {}

/// Response-extension marker stamped on the `503` produced when the inbound
/// request-timeout deadline cancels the handler future.
///
/// The session layer is applied *outer* to the timeout layer, so when the
/// deadline fires it observes the (still-shared) `Session` handle as dirty even
/// though the handler was cancelled mid-flight. Persisting that partial mutation
/// would commit half-finished state — e.g. a login that set the user id but
/// never finished — so `SessionService` checks for this marker and skips the
/// dirty save/destroy when it is present. Only the timeout handler sets it, so
/// ordinary handler-produced `503`s still persist session changes as before.
#[derive(Clone, Copy, Debug)]
pub struct RequestDeadlineCancelled;

/// Build the per-route timeout override table from the top-level routes and any
/// scoped (prefixed) groups. Group routes are keyed by their nested template so
/// the runtime lookup matches [`axum::extract::MatchedPath`].
///
/// `config` is only consulted (behind the `i18n` feature) to expand each
/// locale-prefix-eligible entry under `/{locale}{path}` too — see
/// [`expand_route_timeout_table_for_locale_prefix`].
fn build_route_timeout_table(
    route_list: &[Route],
    scoped_groups: &[ScopedGroup],
    #[cfg_attr(not(feature = "i18n"), allow(unused_variables))] config: &AutumnConfig,
) -> RouteTimeoutTable {
    let mut table: std::collections::HashMap<
        String,
        std::collections::HashMap<http::Method, crate::route::RouteTimeout>,
    > = std::collections::HashMap::new();
    let mut insert = |path: String, method: &http::Method, timeout: crate::route::RouteTimeout| {
        // `Inherit` carries no override, so it never needs a table entry.
        if matches!(timeout, crate::route::RouteTimeout::Inherit) {
            return;
        }
        // Key by (path, effective request method) so an override on one handler
        // never bleeds onto sibling methods sharing the template, while still
        // resolving through a method alias. `RequestTimeoutService` looks up
        // `req.method()`, which differs from the declared method twice:
        //   - axum serves `HEAD` through a `#[get]` handler, so a GET override
        //     must also cover HEAD.
        //   - `#[ws]` records the synthetic `WS` method but mounts a `GET`
        //     handler, so the upgrade arrives as GET.
        // Each (effective method, path) pair is unique across the router, so
        // `insert` cannot lose a competing entry.
        let by_method = table.entry(path).or_default();
        // Key under the same effective verb the router mounts the handler as, so
        // a `#[ws]` override lands on the GET the upgrade actually arrives as
        // (shared with the duplicate-route preflight via `effective_mount_method`
        // so the two mappings can never drift).
        by_method.insert(effective_mount_method(method), timeout);
        // A real `#[get]` is also served for HEAD in axum; a WS upgrade is not,
        // so only expand HEAD for a genuine GET (not the WS→GET alias).
        if *method == http::Method::GET {
            by_method.insert(http::Method::HEAD, timeout);
        }
    };
    for route in route_list {
        insert(route.path.to_owned(), &route.method, route.timeout);
    }
    for group in scoped_groups {
        for route in &group.routes {
            insert(
                join_nested_path(&group.prefix, route.path),
                &route.method,
                route.timeout,
            );
        }
    }
    #[cfg(feature = "i18n")]
    expand_route_timeout_table_for_locale_prefix(&mut table, route_list, &config.i18n);
    std::sync::Arc::new(table)
}

/// `Router::nest("/{locale}", ...)` mounts each locale under a *literal*
/// segment (`"/en"`, `"/es"`, ...), not an axum path parameter, so axum
/// reports `MatchedPath` for a locale-prefixed request as `/{locale}{path}`
/// verbatim (see `join_nested_path_matches_axum_matched_path`, which pins the
/// same literal-concatenation behavior for scoped groups). The base timeout
/// table above is built from `route_list`'s bare, unprefixed paths, so a
/// request that actually matched through a locale nest would never find its
/// override there — this duplicates every locale-prefix-eligible entry under
/// each supported locale's segment too (Codex review). Scoped-group routes are
/// deliberately excluded: they mount after locale-prefix nesting and are never
/// locale-prefixed themselves (see `scoped_group_routes_are_not_locale_prefixed`).
#[cfg(feature = "i18n")]
fn expand_route_timeout_table_for_locale_prefix(
    table: &mut std::collections::HashMap<
        String,
        std::collections::HashMap<http::Method, crate::route::RouteTimeout>,
    >,
    route_list: &[Route],
    i18n: &crate::i18n::I18nConfig,
) {
    if !i18n.locale_prefix_enabled || i18n.supported_locales.is_empty() {
        return;
    }
    for route in route_list {
        if i18n
            .locale_prefix_exclude_exact
            .iter()
            .any(|p| p == route.path)
            || matches_locale_exclude_prefix(route.path, &i18n.locale_prefix_exclude)
        {
            continue;
        }
        let Some(by_method) = table.get(route.path).cloned() else {
            continue;
        };
        for locale in &i18n.supported_locales {
            let prefixed_path = if route.path == "/" {
                format!("/{locale}")
            } else {
                format!("/{locale}{}", route.path)
            };
            table
                .entry(prefixed_path)
                .or_default()
                .extend(by_method.clone());
        }
    }
}

/// Apply the built-in inbound request timeout.
///
/// A single global layer enforces `config.server.timeouts.request_timeout_ms`
/// (the `prod` profile smart-defaults this to 30s) as a per-request wall-clock
/// deadline, with per-route overrides resolved from `route_timeouts` via the
/// matched route template. On expiry the handler returns a framework-standard
/// `503 Service Unavailable` (Problem Details JSON for API clients, the error
/// page for browsers — never a raw tower `BoxError`).
///
/// Streaming responses are exempt by construction: the deadline bounds the time
/// to produce the response head, not the duration of body streaming, so SSE and
/// chunked responses are never interrupted once the head is sent. Long-poll
/// handlers, which block *before* returning the head, are bound by the deadline
/// and must opt out via `timeout = "off"`. WebSocket routes inherit the deadline
/// ([`RouteTimeout::Inherit`](crate::route::RouteTimeout), emitted by `#[ws]`),
/// so it bounds a hung pre-upgrade handshake but never the established socket —
/// that future runs on a separate task via `on_upgrade` and is unbounded by
/// design.
///
/// The layer is a no-op (zero overhead) when the global timeout is disabled and
/// no route declares an `Override`.
///
/// `mirror_cors` makes a synthesized 503 carry the CORS response headers a
/// normal response would. Set it for the main ingress stack, where this layer
/// sits *outside* `CorsLayer` (see the order in `apply_middleware`) so the 503
/// never flows back through it; leave it off for the `/mcp` envelope, whose
/// timeout is applied *inner* to its `CorsLayer` and whose 503 is therefore
/// already CORS-readable.
///
/// Kept as a router-level wrapper for the `/mcp` envelope and this module's
/// unit tests; the main ingress stack composes the layer directly (see
/// `apply_middleware`).
#[cfg(any(test, feature = "mcp"))]
fn apply_request_timeout_middleware(
    router: axum::Router<AppState>,
    config: &AutumnConfig,
    metrics: crate::middleware::MetricsCollector,
    route_timeouts: RouteTimeoutTable,
    mirror_cors: bool,
) -> axum::Router<AppState> {
    let Some(settings) =
        build_request_timeout_settings(config, metrics, route_timeouts, mirror_cors)
    else {
        return router;
    };
    // Both this envelope and `apply_middleware` install the SAME layer type now,
    // so there is nothing left to keep in sync: before #2214 each site had to
    // build its own `axum::middleware::from_fn` closure, because the closure's
    // type (and the opaque future it returned) could not be named across a
    // function boundary, and the two copies could silently drift.
    router.layer(RequestTimeoutLayer::new(settings))
}

/// Everything [`RequestTimeoutService`] needs, resolved once at
/// router-assembly time.
///
/// Held behind an `Arc` by [`RequestTimeoutLayer`] because the produced service
/// is cloned on the request path: every field is individually cheap to clone
/// (`RouteTimeoutTable` and the CORS snapshot are already `Arc`s), but one
/// refcount bump for the whole struct beats four.
struct RequestTimeoutSettings {
    global: Option<Duration>,
    route_timeouts: RouteTimeoutTable,
    metrics: crate::middleware::MetricsCollector,
    cors: Option<std::sync::Arc<crate::config::CorsConfig>>,
}

/// Resolve the request-timeout settings, or `None` when no global timeout is
/// configured and no route declares an override.
///
/// In that case [`apply_request_timeout_middleware`] (the `/mcp` envelope)
/// installs no layer at all, and [`apply_middleware`] contributes an
/// `option_layer` `Either` branch that forwards straight to the inner service —
/// no allocation, no `Route` box, no nesting level. Either way the documented
/// zero-overhead default holds.
///
/// Split out of [`apply_request_timeout_middleware`] so [`apply_middleware`] can
/// place the layer in the composed ingress stack (issue #2193).
fn build_request_timeout_settings(
    config: &AutumnConfig,
    metrics: crate::middleware::MetricsCollector,
    route_timeouts: RouteTimeoutTable,
    mirror_cors: bool,
) -> Option<RequestTimeoutSettings> {
    let global = config
        .server
        .timeouts
        .request_timeout_ms
        .filter(|ms| *ms > 0)
        .map(std::time::Duration::from_millis);
    let has_override = route_timeouts
        .values()
        .flat_map(std::collections::HashMap::values)
        .any(|t| matches!(t, crate::route::RouteTimeout::Override(_)));
    if global.is_none() && !has_override {
        return None;
    }
    if let Some(duration) = global {
        tracing::info!(
            timeout_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            "Inbound request timeout enabled"
        );
    }
    // Snapshot the CORS config once iff we must mirror it onto timeout 503s and
    // any origin is configured (otherwise `CorsLayer` itself is absent).
    let cors = (mirror_cors && !config.cors.allowed_origins.is_empty())
        .then(|| std::sync::Arc::new(config.cors.clone()));
    Some(RequestTimeoutSettings {
        global,
        route_timeouts,
        metrics,
        cors,
    })
}

/// Tower [`Layer`](tower::Layer) applying the framework's per-request deadline.
///
/// Replaces the `axum::middleware::from_fn` closure both call sites used to
/// build. `from_fn` had to `Box::pin` the async block it wrapped — one heap
/// allocation per request, sized by the whole downstream continuation the block
/// captured across its `.await` — and clone its inner service to move it in
/// there. [`RequestTimeoutFuture`] holds `tokio::time::Timeout<S::Future>`
/// (itself a named type) in place instead, so a deadline costs no allocation
/// and an exempt route costs not even a timer (issue #2214).
#[derive(Clone)]
pub struct RequestTimeoutLayer {
    settings: Arc<RequestTimeoutSettings>,
}

impl RequestTimeoutLayer {
    fn new(settings: RequestTimeoutSettings) -> Self {
        Self {
            settings: Arc::new(settings),
        }
    }
}

impl<S> tower::Layer<S> for RequestTimeoutLayer {
    type Service = RequestTimeoutService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestTimeoutService {
            inner,
            settings: Arc::clone(&self.settings),
        }
    }
}

/// Tower [`Service`](tower::Service) produced by [`RequestTimeoutLayer`].
#[derive(Clone)]
pub struct RequestTimeoutService<S> {
    inner: S,
    settings: Arc<RequestTimeoutSettings>,
}

impl<S> RequestTimeoutService<S> {
    /// The deadline that applies to `req`, or `None` when it is exempt.
    ///
    /// Uses borrowed lookups throughout so an exempt or deadline-free route
    /// allocates nothing.
    fn deadline_for<B>(&self, req: &Request<B>) -> Option<Duration> {
        // Internal `autumn build` / ISR regeneration renders drive a
        // `#[static_get]` route directly via `oneshot` and tag the request with
        // `RenderDeadlineExempt` (there is no client connection whose deadline
        // should apply). Skip the deadline for these; live inbound requests to
        // the same route do not carry the marker and are bounded normally.
        if req
            .extensions()
            .get::<crate::static_gen::RenderDeadlineExempt>()
            .is_some()
        {
            return None;
        }

        // Resolve the effective deadline from the matched route template +
        // method.
        let route_timeout = req
            .extensions()
            .get::<axum::extract::MatchedPath>()
            .map(axum::extract::MatchedPath::as_str)
            .and_then(|p| self.settings.route_timeouts.get(p))
            .and_then(|by_method| by_method.get(req.method()))
            .copied()
            .unwrap_or(crate::route::RouteTimeout::Inherit);
        match route_timeout {
            crate::route::RouteTimeout::Disabled => None,
            crate::route::RouteTimeout::Override(d) => Some(d),
            crate::route::RouteTimeout::Inherit => self.settings.global,
        }
    }
}

impl<S> tower::Service<Request<axum::body::Body>> for RequestTimeoutService<S>
where
    S: tower::Service<Request<axum::body::Body>, Response = axum::response::Response>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = RequestTimeoutFuture<S::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<axum::body::Body>) -> Self::Future {
        let Some(duration) = self.deadline_for(&req) else {
            // Exempt (disabled route, or global off with a non-Override route)
            // — no timer and no allocation on this hot path.
            return RequestTimeoutFuture::Unbounded {
                inner: self.inner.call(req),
            };
        };

        // A deadline is active: now it's worth owning the path for the warn log.
        let matched_path = req
            .extensions()
            .get::<axum::extract::MatchedPath>()
            .map(|p| p.as_str().to_owned());
        let request_id = req
            .extensions()
            .get::<crate::middleware::RequestId>()
            .cloned();
        // Capture the request Origin before `req` is consumed so a timeout 503
        // can mirror the CORS headers `CorsLayer` would have added (only when
        // mirroring is enabled — see `apply_request_timeout_middleware`).
        let cors_origin = self
            .settings
            .cors
            .as_ref()
            .and_then(|_| req.headers().get(http::header::ORIGIN).cloned());

        // Build the inner future first, then start the clock, then arm the timer,
        // so `start` and the deadline measure the same interval. The `from_fn`
        // form armed both inside an async block, before the downstream `call`
        // chain ran; here that chain runs during `self.inner.call(req)`, so
        // capturing `start` earlier would make `elapsed_ms` measure a longer span
        // than `timeout_ms`.
        //
        // `tokio::time::timeout` needs a runtime handle, so this `call` must run
        // inside a Tokio runtime. `tower::timeout::Timeout::call` has the same
        // requirement, and every driver in this crate reaches it through
        // `ServiceExt::oneshot`, which calls `call` only from inside a poll.
        let inner = self.inner.call(req);
        let start = std::time::Instant::now();

        RequestTimeoutFuture::Bounded {
            inner: tokio::time::timeout(duration, inner),
            settings: Arc::clone(&self.settings),
            duration,
            matched_path,
            request_id,
            cors_origin,
            start,
        }
    }
}

pin_project_lite::pin_project! {
    /// Future returned by [`RequestTimeoutService`].
    ///
    /// `Unbounded` is the exempt path and is literally the inner service's own
    /// future; `Bounded` wraps it in `tokio::time::Timeout`, which is a named
    /// type, so neither variant is heap-allocated.
    ///
    /// `Elapsed` exists to make the deadline actually *cancel*.
    /// `tokio::time::Timeout::poll` does not drop the future it wraps when the
    /// timer fires — it just reports `Err(Elapsed)` — so a `Bounded` variant
    /// that returned the `503` in place would keep the whole cancelled handler
    /// tree (its database connection guards, its load-shed slot, its webhook
    /// [`ReplayKeyGuard`](crate::webhook)) alive until whatever owns *this*
    /// future is itself dropped, several response layers later. The `from_fn`
    /// form dropped it at the deadline, because its `tokio::time::timeout(..)`
    /// was a `match` scrutinee temporary. Transitioning to `Elapsed` restores
    /// that: `Pin::set` drops the old variant in place, so the handler tree is
    /// released before the `503` starts travelling back out.
    #[project = RequestTimeoutFutureProj]
    pub enum RequestTimeoutFuture<F> {
        Unbounded {
            #[pin]
            inner: F,
        },
        Bounded {
            #[pin]
            inner: tokio::time::Timeout<F>,
            settings: Arc<RequestTimeoutSettings>,
            duration: Duration,
            matched_path: Option<String>,
            request_id: Option<crate::middleware::RequestId>,
            cors_origin: Option<http::HeaderValue>,
            start: std::time::Instant,
        },
        Elapsed {
            response: Option<axum::response::Response>,
        },
    }
}

impl<F, E> std::future::Future for RequestTimeoutFuture<F>
where
    F: std::future::Future<Output = Result<axum::response::Response, E>>,
{
    type Output = Result<axum::response::Response, E>;

    #[allow(
        clippy::expect_used,
        reason = "unreachable: future not polled after Ready"
    )]
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        loop {
            let elapsed = match self.as_mut().project() {
                RequestTimeoutFutureProj::Unbounded { inner } => return inner.poll(cx),
                RequestTimeoutFutureProj::Bounded {
                    inner,
                    settings,
                    duration,
                    matched_path,
                    request_id,
                    cors_origin,
                    start,
                } => match std::task::ready!(inner.poll(cx)) {
                    Ok(response) => return std::task::Poll::Ready(response),
                    Err(_elapsed) => Self::Elapsed {
                        response: Some(deadline_exceeded_response(
                            settings,
                            *duration,
                            matched_path.as_deref(),
                            request_id.as_ref(),
                            cors_origin.as_ref(),
                            *start,
                        )),
                    },
                },
                RequestTimeoutFutureProj::Elapsed { response } => {
                    return std::task::Poll::Ready(Ok(response
                        .take()
                        .expect("RequestTimeoutFuture polled after completion")));
                }
            };
            // Drops the `Bounded` variant — and with it the cancelled handler
            // future the elapsed `Timeout` is still holding — before the `503`
            // leaves this layer.
            self.as_mut().set(elapsed);
        }
    }
}

/// Build the `503` a request that blew its deadline receives, recording the
/// timeout metric and emitting the structured `autumn::timeout` warn on the way.
///
/// Split out of [`RequestTimeoutFuture::poll`] so that hot method stays a
/// dispatch and nothing else.
fn deadline_exceeded_response(
    settings: &RequestTimeoutSettings,
    duration: Duration,
    matched_path: Option<&str>,
    request_id: Option<&crate::middleware::RequestId>,
    cors_origin: Option<&http::HeaderValue>,
    start: std::time::Instant,
) -> axum::response::Response {
    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let route = matched_path.unwrap_or("<unmatched>");
    // Structured telemetry: route template + elapsed time so operators
    // can alert on the (already-counted) timeout event.
    tracing::warn!(
        target: "autumn::timeout",
        route = route,
        elapsed_ms = elapsed_ms,
        timeout_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        request_id = request_id.map(ToString::to_string),
        "inbound request exceeded deadline"
    );
    settings.metrics.record_request_timeout();
    // Return a 503 via the standard error type so the exception-filter
    // and error-page stack negotiate JSON vs HTML and enrich with the
    // request id — no manual Problem Details assembly, no raw BoxError.
    let mut response = crate::error::AutumnError::service_unavailable(RequestDeadlineExceeded {
        timeout_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
    })
    .into_response();
    // Tag the 503 so the outer session layer skips persisting any partial
    // session mutation the cancelled handler made before the deadline.
    response.extensions_mut().insert(RequestDeadlineCancelled);
    // This layer is outside `CorsLayer` in the main stack, so the 503
    // never passes back through it; mirror the CORS headers ourselves so
    // cross-origin browser clients can read the Problem Details body
    // instead of seeing an opaque CORS failure.
    if let Some(cors) = settings.cors.as_deref() {
        mirror_cors_headers(cors, cors_origin, &mut response);
    }
    response
}

struct BuiltIdempotencyLayers {
    route: crate::idempotency::IdempotencyLayer,
    manual: crate::idempotency::IdempotencyLayer,
}

fn build_idempotency_layers(
    config: &AutumnConfig,
    state: &AppState,
) -> Result<Option<BuiltIdempotencyLayers>, RouterBuildError> {
    if !config.idempotency.enabled.unwrap_or(false) {
        return Ok(None);
    }

    let ttl = Duration::from_secs(config.idempotency.ttl_secs);
    let in_flight_ttl = Duration::from_secs(config.idempotency.in_flight_ttl_secs);
    let store: std::sync::Arc<dyn IdempotencyStore> = match config.idempotency.backend {
        crate::config::IdempotencyBackend::Memory => {
            std::sync::Arc::new(MemoryIdempotencyStore::new(ttl))
        }
        #[cfg(feature = "redis")]
        crate::config::IdempotencyBackend::Redis => {
            match crate::idempotency::RedisIdempotencyStore::from_config(&config.idempotency) {
                Ok(s) => std::sync::Arc::new(s),
                Err(e) => return Err(RouterBuildError::InvalidIdempotencyBackend(e)),
            }
        }
        #[cfg(not(feature = "redis"))]
        crate::config::IdempotencyBackend::Redis => {
            return Err(RouterBuildError::InvalidIdempotencyBackend(
                "idempotency backend 'redis' requires the autumn-web 'redis' feature \
                 flag; rebuild with --features redis or switch to backend = \"memory\""
                    .to_owned(),
            ));
        }
    };

    tracing::debug!(
        backend = ?config.idempotency.backend,
        ttl_secs = config.idempotency.ttl_secs,
        in_flight_ttl_secs = config.idempotency.in_flight_ttl_secs,
        "Idempotency-key middleware enabled"
    );

    let base = IdempotencyLayer::new(store)
        .with_ttl(ttl)
        .with_in_flight_ttl(in_flight_ttl)
        .with_metrics(state.metrics.clone())
        .with_entropy(state.entropy_arc());

    Ok(Some(BuiltIdempotencyLayers {
        route: base.clone().replay_through_inner(),
        manual: base.fail_closed_on_replay(),
    }))
}

/// A run of user-registered layers ([`AppBuilder::layer`](crate::app::AppBuilder::layer),
/// [`AppBuilder::static_gate`](crate::app::AppBuilder::static_gate), plugin
/// layers) composed into a SINGLE `tower::Layer`, so an arbitrary number of
/// registrations costs one application instead of one per registration.
///
/// # Why this type exists
///
/// A `tower-layer` tuple needs every member's type at compile time; a `Vec` of
/// registrations does not have that. Erasing each registration to
/// [`ErasedAppLayer`](crate::app::ErasedAppLayer) at registration time makes
/// them homogeneous, and this type folds the homogeneous run by hand. The
/// result is one `Layer` that can sit inside `apply_middleware`'s single merged
/// tuple, so operator layers no longer deepen the framework's per-request
/// clone cascade (#2198) — the framework's overhead becomes a constant instead
/// of a function of how many layers an operator or plugin attached.
///
/// The fold costs exactly one boxing adapter for the whole run (the
/// `ErasedAppService::new` seed), regardless of how many layers it contains.
/// That box does NOT clone on call — `BoxCloneSyncService::call` forwards to
/// the inner service — so it adds no traversal to the cascade either; its
/// runtime cost is one `Box::pin` per request.
///
/// # Why an EMPTY run is still composed in `apply_middleware`
///
/// It is not dead weight there: it is the type boundary that makes the single
/// merged application compile in reasonable time. `tower::util::option_layer`
/// yields `Either<L::Service, S>`, in which the inner service type `S` appears
/// TWICE — so a chain of *n* conditional layers with no erasure between them
/// expands to a type of size `O(2ⁿ)`, and rustc proves `Router::layer`'s
/// `Send`/`Sync`/`Clone` obligations over that expansion. The ingress stack has
/// twelve `option_layer`s. Split at this slot they are 5 above and 7 below
/// (`2⁵ + 2⁷`); merged into one un-erased chain they are `2¹²`, which took
/// rustc over twenty minutes to check `apply_middleware` alone. Dropping this
/// boundary "to save a box when no layer is registered" brings that back.
///
/// `apply_layers_in_registration_order` (the SSG/ISG path) does the opposite
/// and skips an empty run: there the run is applied on its own `Router::layer`
/// call, so there is no long chain to break and the box would buy nothing.
#[derive(Clone)]
struct ComposedRegisteredLayers(Vec<crate::app::ErasedAppLayer>);

impl ComposedRegisteredLayers {
    /// Compose a run of registrations, preserving their registration order.
    fn new(registrations: Vec<crate::app::CustomLayerRegistration>) -> Self {
        Self(registrations.into_iter().map(|reg| reg.layer).collect())
    }
}

impl<S> tower::Layer<S> for ComposedRegisteredLayers
where
    S: tower::Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
{
    type Service = crate::app::ErasedAppService;

    fn layer(&self, inner: S) -> Self::Service {
        // Fold direction. The contract is "first registered ends up outermost on
        // ingress", matching `tower::ServiceBuilder` and the pre-#2198 loop.
        //
        // `Layer::layer(inner)` returns a service that wraps `inner`, so each
        // successive call in this fold produces something strictly more outer,
        // and the last registration visited ends up outermost. Visiting the vec
        // in reverse therefore visits `registrations[0]` last and puts the
        // first-registered layer outermost.
        //
        // The old loop used the same `.rev()` for a different reason: there,
        // `router = reg.apply(router)` made the last `Router::layer` call
        // outermost. Both forms accumulate outward, so both reverse. A
        // `tower-layer` tuple is the form that does not — its first element is
        // outermost. Getting this backwards still compiles, because every layer
        // here is `Request -> Response` with `Error = Infallible`, so only
        // behavioural tests catch it.
        let mut svc = crate::app::ErasedAppService::new(inner);
        for registered in self.0.iter().rev() {
            svc = registered.layer(svc);
        }
        svc
    }
}

/// Re-normalize a group's response body back to `axum::body::Body`.
///
/// Every `Router::layer` call ends in `Route::new`, which maps the produced
/// service's response through `IntoResponse::into_response` — so each of the
/// separate `.layer()` calls this file used to make silently converted a
/// group's exotic response body (e.g. `LogContextLayer`'s `LogContextBody`)
/// back to `axum::body::Body` at the group boundary. Collapsing those calls
/// into one merged tuple removes those implicit conversions, so a boundary
/// between a body-rewrapping group and a member that demands
/// `Response<axum::body::Body>` needs this explicit equivalent.
///
/// It costs nothing measurable: no box, no service clone on call, and the
/// mapping is a fn pointer applied to the response future's output.
///
/// Deliberately a UNIT struct rather than a generic constructor returning
/// `tower::util::MapResponseLayer<fn(Response<B>) -> Response>`: an inference
/// variable for `B` sitting in the middle of the merged tuple makes rustc
/// re-normalize the whole nested `Layer`/`Service` projection chain and pushes
/// this function's type-check into the tens of minutes. With a unit struct,
/// `B` is a projection out of the inner service and never an inference
/// variable at the call site.
#[derive(Clone, Copy)]
struct NormalizeBodyLayer;

impl<S> tower::Layer<S> for NormalizeBodyLayer {
    type Service = NormalizeBody<S>;

    fn layer(&self, inner: S) -> Self::Service {
        NormalizeBody(inner)
    }
}

/// Service half of [`NormalizeBodyLayer`].
#[derive(Clone, Copy)]
struct NormalizeBody<S>(S);

/// Free function (not a closure) so it can be named as a `fn` pointer in
/// `NormalizeBody::Future`, keeping the future un-boxed.
fn into_response_result<B, E>(
    result: Result<http::Response<B>, E>,
) -> Result<axum::response::Response, E>
where
    http::Response<B>: axum::response::IntoResponse,
{
    result.map(axum::response::IntoResponse::into_response)
}

impl<S, B> tower::Service<axum::extract::Request> for NormalizeBody<S>
where
    S: tower::Service<axum::extract::Request, Response = http::Response<B>>,
    http::Response<B>: axum::response::IntoResponse,
{
    type Response = axum::response::Response;
    type Error = S::Error;
    type Future = futures::future::Map<
        S::Future,
        fn(Result<http::Response<B>, S::Error>) -> Result<axum::response::Response, S::Error>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, req: axum::extract::Request) -> Self::Future {
        futures::FutureExt::map(
            self.0.call(req),
            into_response_result::<B, S::Error> as fn(_) -> _,
        )
    }
}

#[allow(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    clippy::too_many_arguments
)]
fn apply_middleware(
    mut router: axum::Router<AppState>,
    config: &AutumnConfig,
    state: &AppState,
    exception_filters: Vec<Arc<dyn ExceptionFilter>>,
    custom_layers: Vec<crate::app::CustomLayerRegistration>,
    #[cfg(feature = "maud")] error_page_renderer: Option<SharedRenderer>,
    session_store: Option<Arc<dyn crate::session::BoxedSessionStore>>,
    route_timeouts: RouteTimeoutTable,
    // Built once by the caller (`build_router_pre_state`) and cloned into the
    // late-mounted `/mcp` envelope too, so both ingress surfaces admit
    // against the SAME shared in-flight counter — constructing a second
    // `LoadShedLayer` here would give `/mcp` its own independent (always-zero)
    // counter that never sheds. See `build_load_shed_layer`.
    load_shed_layer: Option<crate::middleware::LoadShedLayer>,
    // When true (SSG/ISG path), the shadow-mirroring layer is NOT installed
    // here. `try_build_router_with_static_inner` installs a single one OUTSIDE
    // the static-first middleware, so that a request served from the static
    // cache is mirrored too — installing one here as well would double-mirror
    // every dynamic miss against a second, unpublished registry.
    defer_shadow: bool,
) -> Result<axum::Router<AppState>, RouterBuildError> {
    // 404 fallback handler for unmatched routes must be registered BEFORE global middleware
    // so that unmatched routes are still protected by rate limiting, CSRF, CORS, etc.
    router = router.fallback(crate::middleware::error_page_filter::fallback_404_handler);

    // Resolve signing keys once; shared across session and CSRF layers.
    let is_production = matches!(config.profile.as_deref(), Some("prod" | "production"));
    let signing_keys = std::sync::Arc::new(crate::security::config::resolve_signing_keys(
        &config.security.signing_secret,
    ));
    // Only thread signing keys when a secret is configured (or in production where
    // fail_fast already ensures one is present). In dev without a configured secret
    // the ephemeral key is generated per-process — useful but not required.
    let signing_keys_opt: Option<std::sync::Arc<crate::security::config::ResolvedSigningKeys>> =
        if config.security.signing_secret.secret.is_some() || is_production {
            Some(signing_keys)
        } else {
            None
        };

    // ── How the ingress stack is assembled ──────────────────────────────────
    //
    // Each `Router::layer` call re-boxes the whole downstream stack: axum's
    // `Route::layer` ends in `Route::new(..)`, which is
    // `BoxCloneSyncService::new(..)`. N sequential `.layer()` calls build N
    // nested boxes, and `Route::call` deep-clones everything beneath it, so a
    // request descending N levels pays `N + (N-1) + … + 1` heap allocations.
    // Measured against axum 0.8.9 the fit is `13 + N(N+1)/2 + 2N` per request
    // (263 at N = 20, 1388 at N = 50); the same layers in ONE `Router::layer`
    // call cost a flat 16 for any N. See #2193.
    //
    // The layers below are therefore composed into tuples and applied in one
    // call instead of ~16 (#2198 collapsed the last four: the inner group, the
    // user layers, the middle group, and the session). `tower-layer` implements
    // `Layer` for tuples with the FIRST element outermost, so each tuple reads
    // top-to-bottom in ingress order.
    //
    // ⚠ This is the OPPOSITE of repeated `Router::layer` calls, where the LAST
    // call ends up outermost. Reverse the order when moving a layer between the
    // two forms. `tower::ServiceBuilder` matches the tuple form — first-added is
    // outermost — which is why `ComposedRegisteredLayers` folds its run in
    // reverse. Getting this backwards still compiles and type-checks; only
    // behavioural tests catch it.
    //
    // `tower-layer` implements `Layer` for tuples up to 16 elements; the largest
    // group below has 13. Past 16, nest a sub-tuple as one element —
    // `(a, b, (c, d, e), f)` composes identically and still costs one box.
    //
    // Conditional members use `tower::util::option_layer`, which maps `None` to
    // `Identity`: its `Service` is the inner service, wrapped in an `Either` that
    // forwards to it. A disabled layer costs one enum branch per call — no
    // allocation, no `Route` box, no nesting level.

    // Innermost group: everything from the handler out to, but not including, the
    // user-registered layers. Listed OUTERMOST FIRST.
    // Built first because it is one of the two builders that can fail the router
    // build (here the production memory-backend guard for submit tokens; the
    // other is `build_session_layer` below). The infallible builders have side
    // effects — `tracing::info!` lines, and a lazy Redis connection manager for a
    // Redis-backed rate limiter — that must not run on the way to a fail-fast `Err`.
    let submit_token_layer = build_submit_token_layer(config, is_production)?;
    let (body_limit, upload_config) = build_upload_layers(config);
    let trusted_host_policy = TrustedHostPolicy::from_config(config);
    let (rate_limit_layer, rate_limit_principal_keying) = build_rate_limit_layers(config, state);
    let inner_stack = (
        // Insert UploadConfig into extensions so the Multipart extractor can
        // read per-file limits and the allowed MIME-type list.
        axum::Extension(upload_config),
        // Global body-size cap covering JSON, form, raw bytes, and multipart.
        body_limit,
        crate::webhook::WebhookReplayCleanupLayer,
        // Admission control / load shedding (#1006). Outer to MaintenanceLayer so
        // the cheap in-flight-count check runs before maintenance mode's
        // bypass-header/IP-allowlist evaluation. `None` (the default — no
        // `server.max_concurrent_requests` configured) contributes an `Either`
        // branch that forwards straight to the inner service: no allocation and
        // no extra nesting level.
        tower::util::option_layer(load_shed_layer),
        // Maintenance mode (shared construction with the late-mounted `/mcp`
        // envelope — see `build_maintenance_layer`).
        build_maintenance_layer(config, state),
        // Populates RateLimitPrincipal from the verified session identity, so it
        // must run BEFORE (outside) the limiter that keys on it.
        tower::util::option_layer(rate_limit_principal_keying.then(|| {
            axum::middleware::from_fn_with_state(state.clone(), populate_rate_limit_principal)
        })),
        tower::util::option_layer(rate_limit_layer),
        // Method-override rejection filter. The outer `MethodOverrideLayer`,
        // applied at the `axum::serve` boundary so it can rewrite the method
        // before route matching, stamps a [`MethodOverrideRejection`] extension
        // when the override value is invalid or the body was too large to scan.
        // This inner middleware turns that extension into the matching `400`/`413`
        // response, so the rejection flows through the rest of the response stack
        // (security headers, request ids, metrics, error-page filter) instead of
        // bypassing it. Placed outside CSRF so a `BodyTooLarge` on an empty body
        // is not masked by CSRF's missing-token `403`, and a clear
        // `400 invalid _method` outranks "missing CSRF".
        crate::middleware::method_override::MethodOverrideRejectionLayer,
        tower::util::option_layer(build_bot_protection_layer(config)),
        tower::util::option_layer(build_csrf_layer(config, signing_keys_opt.clone())),
        // Inner to the CSRF layer so CSRF is validated first on the request
        // path; a replayed `_submit_token` is still short-circuited even when
        // the request carries a valid `_csrf` (issue #1360, AC #4).
        tower::util::option_layer(submit_token_layer),
        TrustedHostLayer::new(trusted_host_policy),
        tower::util::option_layer(build_ingress_cors_layer(config)),
    );

    // User-registered Tower layers (`AppBuilder::layer`) wrap the group above.
    // They are erased at registration time and folded into
    // `ComposedRegisteredLayers`, so however many an operator or plugin attaches
    // (`Plugin::build` receives the same `AppBuilder`), they occupy ONE slot in
    // the single merged application below rather than one `Router::layer` call
    // each. The `TypeId`/`type_name` that `AppBuilder::has_layer` and
    // `get_layer_types` expose ride along on the registration and survive erasure.
    //
    // With a static dist dir active (SSG/ISG build) these layers are not passed
    // here. `try_build_router_with_static_inner` extracts them and applies them
    // outside the static-first middleware, so they can process pre-rendered
    // responses without creating a session dependency.
    let custom_layer_count = custom_layers.len();
    if custom_layer_count > 0 {
        tracing::debug!(count = custom_layer_count, "Custom Tower layers applied");
    }

    // ── Middle group: outer to the user layers, inner to the session ────────
    //
    // Per-request timeout, inner to RequestId so the request id set by that layer
    // is available when the timeout fires (see `RequestTimeoutService`).
    //
    // Full ingress layer order (outermost → innermost):
    //   TraceContext → AccessLog-fallback (applied in apply_startup_barrier) →
    //   StartupBarrier → Compression → Metrics → ExceptionFilter → ErrorPageContext →
    //   Session → SecurityHeaders → RequestId → LogContext → ServerTiming →
    //   AccessLog-primary → FailureCapture → Reporting → Timeout → Tenancy →
    //   TrustedProxies → [user layers] → BodyLimit/UploadConfig → MethodOverride →
    //   RateLimit → CSRF → CORS → handler
    // `mirror_cors = true`: this layer is outside `CorsLayer`, so its timeout 503
    // must carry CORS headers itself.
    //
    // Known limitation — session store I/O is unbounded. `Session` sits outside
    // this layer, so `store.load` runs before the timer starts and
    // `store.save`/`destroy` after it completes; a stalled session backend can tie
    // up a worker despite `request_timeout_ms`. The placement is deliberate: the
    // timer stays inner to `RequestId` so a timeout 503 and its warn log carry
    // `X-Request-Id`. Bound session-store I/O with a store-level deadline (for
    // example the Redis command timeout); a cancelled inbound request cannot abort
    // an already-issued store call at any layer order.
    //
    // The same applies to the edge layers `App::run` wraps around the finished
    // router at the `axum::serve` boundary (`MethodOverrideLayer`,
    // `TrustedProxiesLayer`): they sit outside `RequestId` and so outside this
    // timer. `MethodOverrideLayer` in particular buffers an HTML form body
    // (`axum::body::to_bytes`, capped at `upload.max_request_size_bytes`) before
    // the inner router runs, so a slow `_method` upload is not bounded by
    // `request_timeout_ms`. Use a server or proxy read timeout for that.
    //
    // `apply_request_timeout_middleware` installs the same layer type for the
    // `/mcp` envelope, so the two cannot drift.
    let timeout_layer =
        build_request_timeout_settings(config, state.metrics.clone(), route_timeouts, true)
            .map(RequestTimeoutLayer::new);

    // Failure-capsule capture (#1598). Outer to the reporting layer, because a
    // request's capture scope must exist before that layer snapshots its context;
    // the scope is what the reporting layer seals into a capsule for a failed
    // request. Off unless `[failure_capture] enabled = true`, since capsules hold
    // real request data. This layer is also the sole arming point for database
    // attribution: the connection-checkout marker fires only under a capture
    // scope, so two apps with different capture settings in one process cannot
    // disturb each other.
    #[cfg(feature = "reporting")]
    let capture_layer = tower::util::option_layer(config.failure_capture.enabled.then(|| {
        // Same filter composition as the log context below, so one
        // `[log] filter_parameters` list governs both.
        let mut capture_filter_parameters = config.log.filter_parameters.clone();
        capture_filter_parameters.extend(crate::encryption::registered_encrypted_column_names());
        let capture_filter = Arc::new(crate::log::filter::ParameterFilter::new(
            &capture_filter_parameters,
            &config.log.unfilter_parameters,
        ));
        // `mut` only on builds that fill in the roles below; a sqlite (or
        // no-`db`) build compiles that block out and records none.
        #[cfg_attr(
            any(not(feature = "db"), feature = "sqlite"),
            allow(
                unused_mut,
                reason = "the role assignment below is compiled out on these builds"
            )
        )]
        let mut capture_settings = crate::capsule::settings_from_config(config);
        // The roles come from the pools the application actually built, not
        // from the configured URLs: a custom `DatabasePoolProvider` may return
        // no pool despite a `primary_url`, or ignore a configured replica (the
        // managed-Postgres provider does exactly that). Recording a role the
        // app does not have would have replay rebuild a shape production never
        // ran. PostgreSQL-only, because that is all replay can reconstruct.
        #[cfg(all(feature = "db", not(feature = "sqlite")))]
        {
            capture_settings.db_roles = crate::capsule::observed_db_roles(
                state.pool().is_some(),
                state.replica_pool().is_some(),
            );
        }
        crate::capsule::CaptureLayer::new(capture_settings, capture_filter)
    }));
    #[cfg(not(feature = "reporting"))]
    let capture_layer = tower::layer::util::Identity::new();

    // Request-scoped log context (#1169). Established for every request, inner
    // to `RequestIdLayer` (so the request id is available to seed it) and outer
    // to tenancy, user layers, and the handler (so all of them, and every
    // `tracing` event they emit, inherit the same correlating context). The
    // filter mirrors the error-page scrubber so sensitive custom fields never
    // enter the context output.
    let mut log_context_filter_parameters = config.log.filter_parameters.clone();
    log_context_filter_parameters.extend(crate::encryption::registered_encrypted_column_names());
    let log_context_filter = Arc::new(crate::log::filter::ParameterFilter::new(
        &log_context_filter_parameters,
        &config.log.unfilter_parameters,
    ));

    // Error-reporting and panic-catch layer. Inner to `RequestIdLayer` so the
    // request id is available when a handler panics, and outer to the timeout,
    // user layers, and handler so their panics become a clean 500 instead of
    // aborting the worker task. That 500 still flows out through the
    // exception-filter chain for HTML negotiation. `config.reporting.enabled` is
    // a constructor argument, not a gate: the layer is installed whenever the
    // `reporting` feature is on, so the panic catch cannot be configured away.
    #[cfg(feature = "reporting")]
    let reporting_layer = crate::reporting::ReportingLayer::new(
        state.error_reporters(),
        config.reporting.enabled,
        config.reporting.sample_rate,
    );
    #[cfg(not(feature = "reporting"))]
    let reporting_layer = tower::layer::util::Identity::new();

    // Structured per-request access log (#999), primary emitter: one INFO event
    // (target `autumn::access`) per served request at the response boundary. Inner
    // to RequestId and LogContext, so the request id is available and the event is
    // emitted inside the request span. Outer to the reporting and timeout layers,
    // so panics-turned-500s and timeout responses are logged with the status the
    // client receives. Emitted responses are marked so the outermost fallback in
    // `apply_startup_barrier` does not double-log; that fallback covers requests
    // which short-circuit before this layer runs.
    let access_log_layer = config
        .log
        .access_log
        .then(|| crate::middleware::AccessLogLayer::new(config.log.access_log_exclude.clone()));

    // Server-Timing response header (#1348). Outer to AccessLogLayer — its
    // `total` metric is therefore the outermost wall-clock measure and is `>=`
    // the access-log `duration_ms` by a few microseconds; both share the same
    // `Instant`-based formula. Opt-in via `[observability] server_timing`;
    // defaults on in dev, off in prod so timings never leak to anonymous prod
    // clients without explicit opt-in.
    let server_timing_layer = crate::config::server_timing_enabled(config)
        .then(|| crate::middleware::ServerTimingLayer::new(true));

    let tenancy_layer = config.tenancy.enabled.then(|| {
        tracing::debug!("Multi-tenancy middleware enabled");
        axum::middleware::from_fn_with_state(state.clone(), crate::tenancy::tenancy_middleware)
    });

    // `security_headers` is applied later as the framework's outermost layer, by
    // `build_router_pre_state` after the gate, so a gate short-circuit still
    // carries HSTS/CSP/nosniff. RequestId stays here, inner to session, so the
    // request id seeds the session, logs, and trace context.
    //
    // TrustedProxiesLayer is the innermost member of this group — immediately
    // outside the user layers — so `ResolvedClientIdentity` is stamped before any
    // middleware reads ClientAddr, ClientHost, or ClientScheme.
    // Listed OUTERMOST FIRST — see the warning at the top of this function.
    let middle_stack = (
        RequestIdLayer::with_entropy(state.entropy_arc()),
        crate::middleware::LogContextLayer::new(log_context_filter),
        tower::util::option_layer(server_timing_layer),
        tower::util::option_layer(access_log_layer),
        capture_layer,
        reporting_layer,
        tower::util::option_layer(timeout_layer),
        tower::util::option_layer(tenancy_layer),
        build_trusted_proxies_layer(config),
    );

    // Pre-clone signing keys for the RYWW middleware (session mode needs to
    // sign/verify the `autumn.ryw` cookie; `signing_keys_opt` is consumed below).
    #[cfg(feature = "db")]
    let signing_keys_for_ryw = signing_keys_opt.clone();

    // The session used to need its own `Router::layer` call: each backend produces
    // a differently-typed `SessionLayer<Store>`, which no fixed tuple member can
    // be. `build_session_layer` monomorphizes it to `SessionLayer<ArcSessionStore>`
    // so it joins the merged tuple below; see that function for the
    // boxed-future-per-store-op cost that buys the nesting level back.
    // Set only when tenancy resolves the tenant from the session: a handler that
    // mutates that session key (an org switch, a tenant-scoped login) needs its
    // deferred idempotency alias keyed by the finalized tenant, not the one
    // resolved before the handler ran.
    let tenancy_session_key = (config.tenancy.enabled && config.tenancy.source == "session")
        .then(|| Arc::<str>::from(config.tenancy.session_key.as_str()));
    let session_layer = crate::session::build_session_layer(
        &config.session,
        config.profile.as_deref(),
        session_store,
        signing_keys_opt,
        &state.entropy_arc(),
        tenancy_session_key,
    )?;
    tracing::debug!(backend = ?config.session.backend, "Session management enabled");

    // Read-your-own-writes middleware: installed only when the mode is not
    // `off`. When active, it scopes a per-request task-local `RequestPin`
    // that generated repository read methods consult at acquire time.
    // Outer to Session, so the task-local also wraps the session store's own
    // reads; the `autumn.ryw` cookie is parsed from raw `Cookie` headers and
    // does not require the Session extractor to have run first.
    #[cfg(feature = "db")]
    let ryw_layer = tower::util::option_layer(
        (config.database.read_your_writes != crate::config::ReadYourWrites::Off).then(|| {
            let ryw_mode = config.database.read_your_writes;
            let window_secs = config.database.pin_after_write_secs;
            let keys = signing_keys_for_ryw;
            if ryw_mode == crate::config::ReadYourWrites::Session && keys.is_none() {
                tracing::warn!(
                    "read_your_writes = \"session\" requires a configured \
                     security.signing_secret to sign the autumn.ryw cookie; \
                     cross-request pinning is disabled until a secret is set"
                );
            }
            let metrics = state.metrics().clone();
            crate::read_your_writes::ReadYourWritesLayer::new(ryw_mode, window_secs, keys, metrics)
        }),
    );
    #[cfg(not(feature = "db"))]
    let ryw_layer = tower::layer::util::Identity::new();

    let is_dev = config
        .profile
        .as_deref()
        .map_or(cfg!(debug_assertions), |p| p == "dev");

    // Error page filter: renders HTML error pages for browser requests.
    // Always registered (uses default renderer if no custom one is provided).

    // When the `maud` feature is enabled, an ErrorPageFilter renders styled HTML
    // error pages for browser requests. Without `maud`, only the
    // ProblemDetailsFilter (JSON error normalization) is installed.
    let mut all_filters: Vec<Arc<dyn ExceptionFilter>> =
        vec![Arc::new(ProblemDetailsFilter { is_dev })];
    #[cfg(feature = "maud")]
    {
        // Encrypted columns (#805) compose into log scrubbing (#697): their names are
        // always scrubbed from trace/error parameter output so ciphertext-backed
        // values never leak through logs even if an app forgets to list them.
        let mut filter_parameters = config.log.filter_parameters.clone();
        filter_parameters.extend(crate::encryption::registered_encrypted_column_names());
        let renderer = error_page_renderer.unwrap_or_else(error_pages::default_renderer);
        let error_page_filter = crate::middleware::error_page_filter::ErrorPageFilter {
            renderer,
            is_dev,
            parameter_filter: crate::log::filter::ParameterFilter::new(
                &filter_parameters,
                &config.log.unfilter_parameters,
            ),
        };
        all_filters.push(Arc::new(error_page_filter));
    }
    all_filters.extend(exception_filters);

    let count = all_filters.len();
    tracing::debug!(
        count,
        "Registered exception filters (including error page filter)"
    );

    // ── Outermost group: outer to the session ───────────────────────────────
    //
    // Response compression is outermost, outside ExceptionFilter, so filters that
    // rebuild the response body (e.g. `ProblemDetailsFilter` normalising
    // `AutumnError`s to JSON Problem Details) run before the body is encoded.
    // Inner to ExceptionFilter, the filter would inherit a `Content-Encoding:
    // gzip` header on its rebuilt uncompressed body and clients would receive
    // uncompressed bytes labeled as gzip. User layers (EtagLayer etc.) stay inner
    // to Compression, so ETags are computed on the uncompressed body.
    //
    // The error page context layer must be inner to the exception filter, so
    // `WantsHtml` is set on the response before the filter inspects it.
    //
    // Full ingress layer order (outermost → innermost). The framework's outermost
    // `SecurityHeadersLayer` and the `static_gate` layers are applied by
    // `build_router_pre_state` after this function returns — and after the MCP
    // dispatch clone is taken — so they are not in this list:
    //   [MethodOverride, TrustedProxies, loopback ConnectInfo — wrapped around
    //   the finished Router by `App::run` at the `axum::serve` boundary, so
    //   they are outside even the startup barrier] ->
    //   TraceContext -> ServerTiming-fallback -> AccessLog-fallback ->
    //   StartupBarrier   (all four applied by `apply_startup_barrier`, which
    //   every entry point calls LAST on the finished router — so this group is
    //   the outermost thing inside the Router) ->
    //   SecurityHeaders (framework outermost within build_router_pre_state) ->
    //   [static_gate layers — applied just inside SecurityHeaders and after the
    //   MCP dispatch clone, outside session and the static cache] ->
    //   [event-bus context, oauth2 interceptor] -> Inspector (dev) ->
    //   dev live-reload (dev)   (all applied in build_router_pre_state) ->
    //   Compression -> ShadowMirror -> Metrics -> ExceptionFilter -> ErrorPageContext ->
    //   ReadYourWrites -> Session -> NormalizeBody ->
    //   RequestId -> LogContext -> ServerTiming -> AccessLog-primary ->
    //   Reporting -> Timeout -> Tenancy -> TrustedProxies ->
    //   [user layers, non-static build — ONE slot however many are registered] ->
    //   UploadConfig -> BodyLimit -> WebhookReplayCleanup -> LoadShed ->
    //   Maintenance -> RateLimitPrincipal -> RateLimit ->
    //   MethodOverrideRejection -> BotProtection -> CSRF -> SubmitToken ->
    //   TrustedHost -> CORS -> [asset cache-control] -> handler
    // Everything from `Compression` through `CORS` is ONE `Router::layer` call:
    // the merged tuple below. `NormalizeBody` is a body-type adapter with no
    // request-path behaviour, listed only so this order reads against that tuple
    // member-for-member; `Compression` carries its own private one (#2371). In the
    // SSG/ISG path the user layers and a second compression layer are applied
    // outside the static-first middleware instead — see
    // `try_build_router_with_static_inner`.
    //
    // Response compression, when `[compression] enabled = true` (off by default),
    // kept outer to `outer_stack` so it stays outside ExceptionFilter. It is
    // wrapped in its own private `(NormalizeBodyLayer, CompressionLayer)` pair
    // rather than an `option_layer` on `CompressionLayer` alone: `option_layer`'s
    // `Either` needs both arms to share one `Response` type, and
    // `CompressionLayer`'s service changes the response BODY type.
    // `NormalizeBodyLayer` folds compression's output back to
    // `axum::response::Response` before `option_layer` compares the arms. This
    // closes the one case #2371 found — a `[compression]`-enabled app paid a full
    // extra `Router::layer` box level, and the quadratic per-request re-clone with
    // it, that every other config-gated member of this tuple already avoided.
    let compression_layer = tower::util::option_layer(config.compression.enabled.then(|| {
        tracing::info!("Response compression enabled (gzip/brotli)");
        (
            NormalizeBodyLayer,
            tower_http::compression::CompressionLayer::new().compress_when(compression_predicate()),
        )
    }));

    // Listed OUTERMOST FIRST — see the warning at the top of this function.
    let outer_stack = (
        // Shadow mirroring (#1653) is the outermost member of this group and
        // therefore INNER to compression: the primary body it tees is the
        // handler's own bytes, which is what the candidate build returns too.
        // Teeing a gzip-encoded body would diff against the candidate's plain
        // one and report every route as divergent.
        //
        // `None` in the SSG/ISG path — see `defer_shadow`.
        tower::util::option_layer(
            (!defer_shadow)
                .then(|| build_shadow_layer(config, state))
                .flatten(),
        ),
        crate::middleware::MetricsLayer::new(state.metrics.clone()),
        ExceptionFilterLayer::new(all_filters),
        crate::middleware::error_page_filter::ErrorPageContextLayer { is_dev },
        ryw_layer,
    );

    // ── The single merged application ───────────────────────────────────────
    //
    // One `Router::layer` call for the whole ingress stack. A `tower-layer` tuple
    // puts its FIRST element OUTERMOST, so this tuple reads in ingress order:
    // outer group, session, middle group, the operator's own layers, then the
    // inner group. That is the order the four separate `.layer()` calls this
    // replaces produced — they ran inner-group-first precisely because repeated
    // calls accumulate outward. Collapsing a run reverses it; see the warning at
    // the top of this function.
    //
    // Each `.layer()` call removed here is a whole `BoxCloneSyncService` nesting
    // level that every request above it deep-clones per call, so the four-to-one
    // collapse is a quadratic-to-linear change, not a constant-factor one (#2193,
    // #2198).
    //
    // `ComposedRegisteredLayers` occupies the user-layer slot unconditionally,
    // even with no layer registered — see its docs for why an empty run still
    // earns its boxing adapter.
    let router = router.layer((
        compression_layer,
        outer_stack,
        session_layer,
        NormalizeBodyLayer,
        middle_stack,
        ComposedRegisteredLayers::new(custom_layers),
        inner_stack,
    ));

    // NOTE: the `static_gate` layers and the framework's outermost
    // `SecurityHeadersLayer` are intentionally NOT applied here. They are applied
    // by `build_router_pre_state` after this function returns and after the MCP
    // dispatch clone is taken, so a `tools/call` replay never traverses the
    // page-cache gate (matching the SSG/ISG path and the documented intent).
    Ok(router)
}

/// Apply a set of user-registered layer registrations in ONE `Router::layer`
/// call, so that the first-registered layer ends up outermost on ingress —
/// matching [`tower::ServiceBuilder`] ordering. Returns the wrapped router.
///
/// An empty run returns the router untouched: no application, and therefore no
/// `BoxCloneSyncService` nesting level and no boxing adapter.
///
/// Used by the SSG/ISG path, which drains the custom layers and the static
/// gates out of `apply_middleware` and applies them outside the static-first
/// middleware instead. The fully-dynamic path composes both runs into larger
/// merged applications (`apply_middleware` and `build_router_pre_state`).
fn apply_layers_in_registration_order(
    router: axum::Router<AppState>,
    layers: Vec<crate::app::CustomLayerRegistration>,
    what: &str,
) -> axum::Router<AppState> {
    let count = layers.len();
    if count == 0 {
        return router;
    }
    tracing::debug!(count, "{what} Tower layers applied");
    router.layer(ComposedRegisteredLayers::new(layers))
}

/// Decide whether `req` clears the trusted-host policy, returning the rejection
/// response when it does not.
///
/// Shared by [`TrustedHostService`] and — through it — every ingress path, so
/// the decision lives in exactly one place. Returns `None` for "let it
/// through", which is the overwhelmingly common answer and costs no allocation
/// at all: the host string is only owned on the branch that has to compare it.
fn trusted_host_rejection<B>(
    req: &Request<B>,
    policy: &TrustedHostPolicy,
) -> Option<axum::response::Response> {
    let path = req.uri().path();
    if (req.method() == http::Method::GET || req.method() == http::Method::HEAD)
        && policy.probe_bypass_paths.contains(path)
    {
        return None;
    }
    let authority = req.uri().authority().map(http::uri::Authority::as_str);
    let host_header = req
        .headers()
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok());
    let raw_host = authority.or(host_header);
    let parsed_host = raw_host.and_then(extract_host_without_port);
    let host = parsed_host
        .map(str::to_ascii_lowercase)
        .map(|h| h.trim_end_matches('.').to_owned())
        .filter(|h| !h.is_empty());
    let host_source_present = raw_host.is_some();
    if host.is_none() && !host_source_present && policy.allow_missing_host {
        return None;
    }
    if host.as_deref().is_some_and(|host| policy.allows_host(host)) {
        return None;
    }
    tracing::warn!(host = ?host, "trusted host rejected request");
    let body = crate::error::problem_details_json_string(
        StatusCode::BAD_REQUEST,
        "Invalid Host header",
        None,
        None,
        None,
        None,
        true,
    );
    Some(
        (
            StatusCode::BAD_REQUEST,
            [(http::header::CONTENT_TYPE, "application/problem+json")],
            body,
        )
            .into_response(),
    )
}

/// Tower [`Layer`](tower::Layer) enforcing [`TrustedHostPolicy`] on the ingress
/// path.
///
/// This used to be an `axum::middleware::from_fn` closure. It is a hand-rolled
/// service now because `from_fn` `Box::pin`s the future of whatever it wraps —
/// one heap allocation per request, sized by everything the wrapped async block
/// captures across its `.await`, which for a layer this far out is the whole
/// downstream continuation — plus a `self.inner.clone()` that deep-clones the
/// erased stack beneath it. Neither cost depended on whether a request was
/// actually rejected (issue #2214).
#[derive(Clone, Debug)]
pub struct TrustedHostLayer {
    policy: TrustedHostPolicy,
}

impl TrustedHostLayer {
    pub(crate) const fn new(policy: TrustedHostPolicy) -> Self {
        Self { policy }
    }
}

impl<S> tower::Layer<S> for TrustedHostLayer {
    type Service = TrustedHostService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TrustedHostService {
            inner,
            policy: self.policy.clone(),
        }
    }
}

/// Tower [`Service`](tower::Service) produced by [`TrustedHostLayer`].
#[derive(Clone, Debug)]
pub struct TrustedHostService<S> {
    inner: S,
    policy: TrustedHostPolicy,
}

impl<S, ReqBody> tower::Service<Request<ReqBody>> for TrustedHostService<S>
where
    S: tower::Service<Request<ReqBody>, Response = axum::response::Response>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = crate::middleware::short_circuit::ShortCircuitFuture<S::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        use crate::middleware::short_circuit::ShortCircuitFuture;
        trusted_host_rejection(&req, &self.policy).map_or_else(
            || ShortCircuitFuture::forward(self.inner.call(req)),
            ShortCircuitFuture::short_circuit,
        )
    }
}

pub fn extract_host_without_port(header: &str) -> Option<&str> {
    let host = header.trim();
    if host.is_empty() {
        return None;
    }
    if host.starts_with('[') {
        let end = host.find(']')?;
        let literal = host.get(1..end)?;
        if literal.is_empty() || literal.parse::<std::net::IpAddr>().is_err() {
            return None;
        }

        let remainder = host.get(end + 1..)?;
        if remainder.is_empty() {
            return Some(literal);
        }

        let maybe_port = remainder.strip_prefix(':')?;
        if !maybe_port.is_empty() && maybe_port.chars().all(|c| c.is_ascii_digit()) {
            return Some(literal);
        }

        return None;
    }
    let Some((candidate, maybe_port)) = host.rsplit_once(':') else {
        return Some(host);
    };
    if candidate.contains(':') {
        // unbracketed IPv6 literal; keep host verbatim
        return Some(host);
    }
    if !maybe_port.is_empty()
        && maybe_port.chars().all(|c| c.is_ascii_digit())
        && !candidate.is_empty()
    {
        Some(candidate)
    } else {
        None
    }
}

/// Build the router with optional static-file-first serving.
///
/// If `dist_dir` is `Some` and contains a valid `manifest.json`, the
/// returned router intercepts GET/HEAD requests whose path appears in
/// the manifest and serves pre-built HTML directly — before the dynamic
/// router runs.  This matches Next.js SSG/ISR semantics where static
/// pages always win over dynamic handlers.
///
/// Requests not in the manifest (including non-GET/HEAD methods) fall
/// through to the dynamic router unchanged.
///
/// When `dist_dir` is `None` or the manifest is missing, the returned
/// router is identical to [`build_router`].
///
/// This function is public primarily for integration testing.
///
/// # Panics
///
/// Panics when framework router assembly encounters invalid configuration.
/// Use [`try_build_router_with_static`] to handle configuration errors
/// explicitly.
#[allow(dead_code)]
pub fn build_router_with_static(
    route_list: Vec<Route>,
    config: &AutumnConfig,
    state: AppState,
    dist_dir: Option<&std::path::Path>,
) -> axum::Router {
    try_build_router_with_static(route_list, config, state, dist_dir)
        .unwrap_or_else(|error| panic!("invalid router configuration: {error}"))
}

/// Checked variant of [`build_router_with_static`] that returns configuration
/// errors instead of panicking.
///
/// # Errors
///
/// Returns [`RouterBuildError`] when router assembly encounters invalid
/// framework configuration, such as an unusable session backend.
#[allow(dead_code)]
pub fn try_build_router_with_static(
    route_list: Vec<Route>,
    config: &AutumnConfig,
    state: AppState,
    dist_dir: Option<&std::path::Path>,
) -> Result<axum::Router, RouterBuildError> {
    try_build_router_with_static_inner(
        route_list,
        config,
        state,
        dist_dir,
        RouterContext {
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
}

#[allow(clippy::too_many_lines)]
pub fn try_build_router_with_static_inner(
    route_list: Vec<Route>,
    config: &AutumnConfig,
    state: AppState,
    dist_dir: Option<&std::path::Path>,
    mut ctx: RouterContext,
) -> Result<axum::Router, RouterBuildError> {
    let startup_barrier_state = state.clone();

    let Some(dist) = dist_dir else {
        let app_router = try_build_router_inner(route_list, config, state, ctx)?;
        return Ok(apply_startup_barrier(
            app_router,
            config,
            &startup_barrier_state,
        ));
    };

    let Some(layer) = crate::static_gen::StaticFileLayer::new(dist) else {
        tracing::debug!(
            dist = %dist.display(),
            "No valid manifest.json in dist dir; skipping static file layer"
        );
        let app_router = try_build_router_inner(route_list, config, state, ctx)?;
        return Ok(apply_startup_barrier(
            app_router,
            config,
            &startup_barrier_state,
        ));
    };

    for (route, entry) in &layer.manifest().routes {
        tracing::debug!(
            route = %route,
            file = %entry.file,
            revalidate = ?entry.revalidate,
            "Static route"
        );
    }

    // Extract user layers before building the inner router. They are applied
    // outside the static-first middleware, and outside session, so that:
    //   • User layers (e.g. compression) can process pre-rendered responses.
    //   • Static serving stays available when the session backend is down.
    //   • ISR regeneration uses the inner router (no user layers), so re-rendered
    //     pages are saved as raw HTML rather than pre-transformed.
    //
    // Known limitation — `request_timeout_ms` does not bound these outer layers.
    // The per-request timeout lives inside `inner_router`, applied by
    // `apply_middleware` inner to `RequestId`. `custom_layers` and
    // `static_gate_layers` are reapplied outside the static-first middleware
    // below, so they, and the static cache lookup itself, run before the timer
    // starts. With a `dist` manifest active, a hung async `static_gate` (e.g.
    // remote JWT/IdP validation) or custom layer is therefore unbounded, unlike
    // the non-static path. This is the same trade-off as the session-store and
    // edge-layer limitations documented in `apply_middleware`: pulling the timer
    // out here would place it outside `RequestId` and lose `X-Request-Id` on the
    // timeout 503, double-time dynamic misses, and apply a global deadline to
    // cached hits with no route-table entry. Bound auth/tenant work in a
    // `static_gate` with a layer-level or server/proxy read timeout instead.
    //
    // Compute the idempotency flag now, while `custom_layers` is still populated,
    // then drain it. `build_router_pre_state` would otherwise see an empty list
    // and treat opaque layers as absent when selecting per-route idempotency
    // behaviour. Pre-static gate layers count too: a `static_gate` used as a
    // JWT/stateless auth layer is an opaque app layer for idempotency purposes,
    // because idempotency keys exclude `Authorization`, so without fail-closed
    // replay a second principal with the same key and body could receive the
    // first principal's cached mutation. Include both lists before either drains.
    let opaque_present = Some(
        custom_layers_require_fail_closed_idempotency(&ctx.custom_layers)
            || custom_layers_require_fail_closed_idempotency(&ctx.static_gate_layers),
    );
    let custom_layers = std::mem::take(&mut ctx.custom_layers);

    // #1384: the ambient-locale layer must not drain out with the rest. It runs
    // `Locale::from_request_parts`, whose session step reads the signed session,
    // and everything drained here is applied outside the static-first middleware
    // — that is, outside `SessionLayer`. Out there the session extension does not
    // exist yet, so a locale persisted by the documented `set_locale_in_session`
    // switcher would be invisible and content would resolve from
    // `Accept-Language` instead, disagreeing with the UI chrome on the same page.
    // A handler that deliberately takes no `Locale` argument — the point of the
    // feature — never runs an extractor later to correct it.
    //
    // Putting it back on the inner router's context lands it in
    // `apply_middleware`'s merged tuple, which is inside `session_layer` on both
    // this path and the fully-dynamic one. The bundle `Extension` still drains
    // out and stays outer, so the layer can read it.
    //
    // Shadowed rather than mutated in place: with the `i18n` feature off this
    // block vanishes, and a `let mut` the remaining code never reassigns fails
    // `-D warnings` in every non-unified build (`-p autumn-web`, the sqlite
    // lane). A `--workspace` build hides that, because another member turns
    // `i18n` on and Cargo unifies it.
    #[cfg(feature = "i18n")]
    let custom_layers = {
        let (session_scoped, outside): (Vec<_>, Vec<_>) = custom_layers
            .into_iter()
            .partition(|r| r.type_id == std::any::TypeId::of::<crate::i18n::AmbientLocaleLayer>());
        ctx.custom_layers = session_scoped;
        outside
    };

    // Pre-static gate layers (AppBuilder::static_gate) are likewise extracted
    // and applied OUTSIDE the static-first middleware (the outermost layer of
    // all), so they run before the static cache lookup serves a pre-rendered
    // page. Draining them here keeps build_router_pre_state from applying them
    // to the inner router (which would place them inside the static middleware
    // and defeat the gate for cached hits).
    let static_gate_layers = std::mem::take(&mut ctx.static_gate_layers);

    // SSG/ISG path: a single SecurityHeadersLayer is applied OUTSIDE the
    // static-first middleware below (wrapping cached pages, dynamic misses, and
    // the gate), so the inner router must NOT apply its own — hence `true`.
    let inner_router =
        build_router_pre_state(route_list, config, &state, ctx, opaque_present, true)?;

    // Attach the inner router for ISR background regeneration. Because user
    // layers are excluded, re-renders produce raw HTML (no compression, etc.)
    // that is then saved to disk and served with user-layer processing applied
    // at request time.
    let has_isr = layer
        .manifest()
        .routes
        .values()
        .any(|e| e.revalidate.is_some());
    let layer = if has_isr {
        // The inner router defers `SecurityHeadersLayer` to the single outer
        // application (see `defer_security_headers`), but ISR background
        // regeneration drives this router directly and never reaches that layer.
        // `SecurityHeadersLayer` also injects `CspNonce` into request extensions,
        // so without it a handler using the `CspNonce` extractor would 500 during
        // regeneration and the stale file would never refresh. Re-attach it on
        // the regeneration router only. Its response headers are discarded — only
        // the rendered HTML body is persisted — so live-request headers are
        // unaffected and no duplicate-header or nonce conflict arises.
        let regen_router = inner_router
            .clone()
            .layer(crate::security::SecurityHeadersLayer::from_config(
                &config.security.headers,
            ))
            .with_state(state.clone());
        layer.with_router(regen_router)
    } else {
        layer
    };
    let layer = Arc::new(layer);

    // Static-first serving: intercept GET/HEAD requests whose path is in the
    // manifest and serve pre-built HTML directly, before the dynamic router and
    // session layer run. Static pages stay available when the session backend is
    // down. Requests not in the manifest, including other methods, fall through
    // to the dynamic router unchanged. `resolve()` checks ISR staleness: a stale
    // page is served immediately while regeneration runs in the background
    // (stale-while-revalidate).
    let static_layer = layer;
    let mut router: axum::Router<AppState> = inner_router.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let static_layer = static_layer.clone();
            async move {
                let is_get = req.method() == http::Method::GET;
                let is_head = req.method() == http::Method::HEAD;
                if is_get || is_head {
                    let path = req.uri().path();
                    // Normalize trailing slash: /about/ → /about (but keep / as /)
                    let normalized = if path.len() > 1 && path.ends_with('/') {
                        &path[..path.len() - 1]
                    } else {
                        path
                    };
                    if let Some(hit) = static_layer.resolve_entry(normalized)
                        && let Ok(contents) = tokio::fs::read(&hit.file_path).await
                    {
                        // #1832: `resolve_entry` returns the Content-Type the
                        // manifest recorded at generation time (see
                        // `static_gen::resolved_content_type` for the ordering
                        // and the legacy fallback). It is a `HeaderValue`, so
                        // the builder below cannot fail on manifest content.
                        // An accurate type matters here because the compression
                        // layer is applied outside this middleware and
                        // negotiates gzip/brotli by content type: it encodes
                        // compressible SSG pages and leaves binary assets alone.
                        let body = if is_head {
                            axum::body::Body::empty()
                        } else {
                            axum::body::Body::from(contents)
                        };
                        return http::Response::builder()
                            .status(http::StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, hit.content_type)
                            .body(body)
                            .expect("infallible response builder");
                    }
                }
                next.run(req).await
            }
        },
    ));

    // Apply user layers OUTSIDE the static middleware so they wrap it and can
    // process both static and dynamic responses (e.g. compress the HTML on
    // the way out). The first registered layer ends up outermost — matching
    // `tower::ServiceBuilder` ordering; the fold that gets it there lives in
    // `apply_layers_in_registration_order` / `ComposedRegisteredLayers`.
    router = apply_layers_in_registration_order(
        router,
        custom_layers,
        "Custom (outside static middleware)",
    );

    // Shadow mirroring (#1653) sits here — outside the static-first middleware
    // and inside compression — rather than in `apply_middleware`, which is why
    // that call passed `defer_shadow = true`. A request the static cache answers
    // never reaches the inner router, so a layer installed there would see only
    // dynamic misses: a shadow run over an SSG/ISG app would report clean while
    // every pre-rendered page the candidate generated differently went
    // uncompared. Outer to the user layers and inner to compression, matching its
    // position in the dynamic path.
    if let Some(shadow) = build_shadow_layer(config, &state) {
        router = router.layer(shadow);
    }

    // Compression is applied outside the static-first middleware too, so
    // pre-rendered HTML that `StaticFileLayer` serves without reaching
    // `inner_router` is still compressed. This mirrors the placement in
    // `apply_middleware` for the dynamic-only path.
    router = apply_compression_middleware(router, config);

    // Pre-static gate layers run before the static cache lookup (they wrap the
    // static-first middleware) so they can redirect / reject a request before a
    // cached SSG/ISG page is served. They are applied INNER to the
    // SecurityHeadersLayer below so that a gate's short-circuit response
    // (redirect / 401) still carries the framework security headers (HSTS/CSP,
    // etc.) — matching the headers a normal cached or dynamic response gets.
    router = apply_layers_in_registration_order(
        router,
        static_gate_layers,
        "Pre-static gate (outside static middleware)",
    );

    // Security headers are applied OUTERMOST so they wrap both cached pages and
    // any gate short-circuit response. This is the SINGLE application for the
    // SSG/ISG path: the inner router skips it (build_router_pre_state is called
    // with `defer_security_headers = true`), so dynamic misses are not
    // double-wrapped (which would break CSP nonces).
    let router = router.layer(crate::security::SecurityHeadersLayer::from_config(
        &config.security.headers,
    ));

    Ok(apply_startup_barrier(
        router.with_state(state),
        config,
        &startup_barrier_state,
    ))
}

#[derive(Clone)]
pub struct StartupBarrierState {
    app_state: AppState,
    // Canonical exact-match probe/health paths (`probe_bypass_paths`), the
    // single source of truth shared with `TrustedHostPolicy` and the
    // maintenance/load-shed gates — see that function's doc comment.
    probe_paths: Vec<String>,
    actuator_paths: Vec<String>,
    actuator_subtree_paths: Vec<String>,
}

impl StartupBarrierState {
    fn from_config(config: &AutumnConfig, app_state: &AppState) -> Self {
        let actuator_subtree_paths = if config.actuator.sensitive {
            vec![crate::actuator::actuator_route_path(
                &config.actuator.prefix,
                "/loggers",
            )]
        } else {
            Vec::new()
        };

        Self {
            app_state: app_state.clone(),
            probe_paths: probe_bypass_paths(config),
            actuator_paths: crate::actuator::actuator_endpoint_paths(
                &config.actuator.prefix,
                config.actuator.sensitive,
                config.actuator.prometheus,
            ),
            actuator_subtree_paths,
        }
    }

    fn allows_path(&self, path: &str) -> bool {
        self.probe_paths.iter().any(|allowed| path == allowed)
            || self.actuator_paths.iter().any(|allowed| path == allowed)
            || self
                .actuator_subtree_paths
                .iter()
                .any(|allowed| path_matches_route_prefix(path, allowed))
    }
}

fn apply_startup_barrier(
    router: axum::Router,
    config: &AutumnConfig,
    state: &AppState,
) -> axum::Router {
    let barrier_state = StartupBarrierState::from_config(config, state);

    // These four are the outermost layers on every production build path, so they
    // are composed into one tuple and applied with a single `Router::layer` call.
    // Four separate calls would nest four `BoxCloneSyncService` levels around
    // every route, and axum deep-clones that nest on each request (#2193).
    //
    // ⚠ Tuple order is OUTERMOST FIRST — the opposite of consecutive
    // `Router::layer` calls, where the last call ends up outermost.
    //
    // W3C Trace Context propagation wraps the startup barrier and the
    // static-first middleware above it, so short-circuit responses — startup 503s
    // and pre-built static hits — still extract the incoming `traceparent` and
    // inject the current context into the response. It is applied here rather
    // than inside `apply_middleware` because those outer wrappers can return
    // without invoking the inner router. Outer to AccessLog, so the access event
    // is emitted while the trace context is current.
    #[cfg(feature = "telemetry-otlp")]
    let trace_context = crate::middleware::TraceContextLayer;
    #[cfg(not(feature = "telemetry-otlp"))]
    let trace_context = tower::layer::util::Identity::new();

    // Server-Timing fallback (#1348), applied outside the startup barrier, the
    // static-first (SSG/ISR) middleware, the session layer, and the late MCP
    // merge — the short-circuit paths the primary `ServerTimingLayer` in
    // `apply_middleware` never sees. Gated on the same `server_timing_enabled`
    // resolver as the primary. It appends only for responses missing the
    // `ServerTimingEmitted` marker, so a request reaching the primary carries one
    // `total` and a short-circuit gets its `total` here.
    let server_timing_fallback = crate::config::server_timing_enabled(config)
        .then(|| crate::middleware::ServerTimingLayer::fallback(true));

    // Access-log fallback (#999), applied outside the startup barrier, the
    // static-first (SSG/ISR) middleware, the session layer, and the
    // exception-filter chain. Every production build path funnels through this
    // function, including after the late MCP merge. It emits only for responses
    // the primary in-stack layer never saw, checking the `AccessLogEmitted`
    // marker, so startup 503s, pre-built static hits, session-store outage 503s,
    // and MCP requests get an access line too. Those short-circuits never ran
    // `RequestIdLayer`, so the fallback reads `x-request-id` from the response
    // when present and logs without a request id otherwise.
    let access_log_fallback = config.log.access_log.then(|| {
        crate::middleware::AccessLogLayer::fallback(config.log.access_log_exclude.clone())
    });

    router.layer((
        trace_context,
        tower::util::option_layer(server_timing_fallback),
        tower::util::option_layer(access_log_fallback),
        StartupBarrierLayer::new(barrier_state),
    ))
}

/// Tower [`Layer`](tower::Layer) for the startup readiness barrier: requests are refused with
/// `503 Service is still starting up` until the app reports startup complete,
/// except on the paths the barrier lets through (probes, actuator).
///
/// A hand-rolled service rather than an `axum::middleware::from_fn`: this is the
/// outermost layer inside the `Router`, so `from_fn`'s per-request `Box::pin`
/// captured the entire downstream continuation and its `self.inner.clone()`
/// deep-cloned the whole erased stack — on every request, for a check that
/// passes on every request after the first few seconds of process life
/// (issue #2214).
///
/// The state is held behind an `Arc` because the produced service is cloned on
/// the request path — once per traversal of the stack above it — and
/// `StartupBarrierState` owns an `AppState` plus three `Vec<String>` path lists.
/// Holding it by value would deep-copy all three on every one of those clones,
/// which is the cost #2193 removed elsewhere in this stack.
#[derive(Clone)]
pub struct StartupBarrierLayer {
    state: Arc<StartupBarrierState>,
}

impl StartupBarrierLayer {
    pub(crate) fn new(state: StartupBarrierState) -> Self {
        Self {
            state: Arc::new(state),
        }
    }
}

impl<S> tower::Layer<S> for StartupBarrierLayer {
    type Service = StartupBarrierService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        StartupBarrierService {
            inner,
            state: Arc::clone(&self.state),
        }
    }
}

/// Tower [`Service`](tower::Service) produced by [`StartupBarrierLayer`].
#[derive(Clone)]
pub struct StartupBarrierService<S> {
    inner: S,
    state: Arc<StartupBarrierState>,
}

impl<S, ReqBody> tower::Service<Request<ReqBody>> for StartupBarrierService<S>
where
    S: tower::Service<Request<ReqBody>, Response = axum::response::Response>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = crate::middleware::short_circuit::ShortCircuitFuture<S::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        use crate::middleware::short_circuit::ShortCircuitFuture;
        if crate::app::is_static_build_mode()
            || self.state.app_state.probes().is_startup_complete()
            || self.state.allows_path(req.uri().path())
        {
            ShortCircuitFuture::forward(self.inner.call(req))
        } else {
            ShortCircuitFuture::short_circuit(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service is still starting up",
                )
                    .into_response(),
            )
        }
    }
}

pub fn path_matches_route_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Build a `tower_http::cors::CorsLayer` from the framework's [`crate::config::CorsConfig`].
///
/// Called only when `config.cors.allowed_origins` is non-empty.
pub fn build_cors_layer(cors: &crate::config::CorsConfig) -> tower_http::cors::CorsLayer {
    use http::header::HeaderName;
    use tower_http::cors::{AllowOrigin, CorsLayer};

    let layer = if cors.allowed_origins.iter().any(|o| o == "*") {
        CorsLayer::new().allow_origin(AllowOrigin::any())
    } else {
        let origins: Vec<http::HeaderValue> = cors
            .allowed_origins
            .iter()
            .filter_map(|o| match o.parse() {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(origin = %o, error = %e, "CORS: ignoring malformed allowed_origin");
                    None
                }
            })
            .collect();
        CorsLayer::new().allow_origin(origins)
    };

    let methods: Vec<http::Method> = cors
        .allowed_methods
        .iter()
        .filter_map(|m| match m.parse() {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(method = %m, error = %e, "CORS: ignoring malformed allowed_method");
                None
            }
        })
        .collect();

    let headers: Vec<HeaderName> = cors
        .allowed_headers
        .iter()
        .filter_map(|h| match h.parse() {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(header = %h, error = %e, "CORS: ignoring malformed allowed_header");
                None
            }
        })
        .collect();

    layer
        .allow_methods(methods)
        .allow_headers(headers)
        .allow_credentials(cors.allow_credentials)
        .max_age(std::time::Duration::from_secs(cors.max_age_secs))
}

/// Mirror onto a timeout-generated 503 the CORS response headers `CorsLayer`
/// would add to a normal (non-preflight) response.
///
/// In the main ingress stack the per-request timeout layer sits *outside*
/// `CorsLayer` (see the layer order in `apply_middleware`), so a 503 it
/// synthesizes on expiry never flows back through `CorsLayer`. Without this a
/// cross-origin browser client sees an opaque CORS failure instead of the
/// documented Problem Details 503. Only the simple-response subset is needed:
/// the resolved `Access-Control-Allow-Origin` (with `Vary: origin` when it is
/// reflected) and `Access-Control-Allow-Credentials`. Preflight (OPTIONS)
/// requests are answered by `CorsLayer` directly and never reach the timer.
/// Mirror the `Access-Control-*` response headers a real `CorsLayer` would
/// have added, onto a `response` synthesized by a layer that sits outside
/// (outer to) `CorsLayer` in the ingress stack — so its 503 is CORS-readable
/// instead of the client seeing an opaque CORS failure. Shared by the
/// per-request timeout middleware and [`crate::middleware::LoadShedLayer`],
/// the two admission-style gates that can short-circuit before `CorsLayer`
/// runs.
pub fn mirror_cors_headers(
    cors: &crate::config::CorsConfig,
    origin: Option<&http::HeaderValue>,
    response: &mut axum::response::Response,
) {
    use http::header;
    let allow_any = cors.allowed_origins.iter().any(|o| o == "*");
    let allow_origin = if allow_any {
        Some(http::HeaderValue::from_static("*"))
    } else {
        // Echo the request Origin iff it is in the configured allowlist, exactly
        // as `CorsLayer` does for a reflected origin.
        origin.and_then(|value| {
            let value_str = value.to_str().ok()?;
            cors.allowed_origins
                .iter()
                .any(|allowed| allowed == value_str)
                .then(|| value.clone())
        })
    };
    let Some(allow_origin) = allow_origin else {
        // Origin missing or not allowed: a real `CorsLayer` would add nothing.
        return;
    };
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allow_origin);
    if !allow_any {
        // A reflected origin makes the response origin-dependent; mirror the
        // `Vary: origin` `CorsLayer` adds so shared caches don't serve it to a
        // different origin.
        headers.insert(header::VARY, http::HeaderValue::from_static("origin"));
    }
    if cors.allow_credentials {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            http::HeaderValue::from_static("true"),
        );
    }
}

#[cfg(feature = "htmx")]
pub async fn htmx_handler() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [
            (http::header::CONTENT_TYPE, "application/javascript"),
            (
                http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ),
        ],
        crate::htmx::HTMX_JS,
    )
        .into_response()
}

/// Gzip/brotli encodings of a compile-time-constant CSS body, computed once
/// per process (via a call-site-owned [`std::sync::OnceLock`], see
/// [`flash_css_handler`]/[`widgets_css_handler`]) rather than redone on every
/// request — the bytes never change, so recompressing them per-request would
/// burn CPU for a byte-identical result each time.
#[cfg(any(feature = "flash", feature = "maud"))]
struct PrecompressedCss {
    gzip: bytes::Bytes,
    brotli: bytes::Bytes,
}

#[cfg(any(feature = "flash", feature = "maud"))]
impl PrecompressedCss {
    fn compute(body: &'static str) -> Self {
        use std::io::Write as _;

        let mut gzip_encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip_encoder
            .write_all(body.as_bytes())
            .expect("in-memory gzip encoding cannot fail");
        let gzip = gzip_encoder
            .finish()
            .expect("in-memory gzip encoding cannot fail");

        let mut brotli_writer = brotli::CompressorWriter::new(Vec::new(), 4096, 11, 22);
        brotli_writer
            .write_all(body.as_bytes())
            .expect("in-memory brotli encoding cannot fail");
        let brotli = brotli_writer.into_inner();

        Self {
            gzip: gzip.into(),
            brotli: brotli.into(),
        }
    }
}

/// `true` when the request's `Accept-Encoding` header accepts `coding`
/// (case-insensitive, comma-separated, honoring an explicit `q=0` opt-out
/// per RFC 7231 §5.3.4). A minimal parser rather than a full content-
/// negotiation crate, since only `gzip`/`br` ever need checking here.
#[cfg(any(feature = "flash", feature = "maud"))]
fn accepts_encoding(headers: &http::HeaderMap, coding: &str) -> bool {
    let Some(value) = headers
        .get(http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    value.split(',').any(|part| {
        let mut segments = part.split(';');
        let name = segments.next().unwrap_or("").trim();
        name.eq_ignore_ascii_case(coding)
            && segments
                .find_map(|q| q.trim().strip_prefix("q="))
                .and_then(|q| q.parse::<f32>().ok())
                .is_none_or(|q| q > 0.0)
    })
}

/// Serves a framework-owned, compile-time-constant CSS asset: same-origin,
/// immutably cached, conditional-GET aware (a strong `ETag` hashed from
/// `body`, so a revalidating client gets a bodyless `304` instead of the
/// full asset), and served pre-compressed from `precompressed` when the
/// client's `Accept-Encoding` allows it — computed once per process, not
/// per request. Shared by every framework CSS route ([`flash_css_handler`],
/// [`widgets_css_handler`]) so the caching/content-type/compression policy
/// lives in one place.
#[cfg(any(feature = "flash", feature = "maud"))]
fn static_css_response(
    headers: &http::HeaderMap,
    body: &'static str,
    precompressed: &'static PrecompressedCss,
) -> axum::response::Response {
    use crate::etag::IntoETag as _;
    use axum::response::IntoResponse;

    let (encoded_body, content_encoding): (axum::body::Body, Option<&'static str>) =
        if accepts_encoding(headers, "br") {
            (precompressed.brotli.clone().into(), Some("br"))
        } else if accepts_encoding(headers, "gzip") {
            (precompressed.gzip.clone().into(), Some("gzip"))
        } else {
            (body.into(), None)
        };

    let mut response_headers = http::HeaderMap::new();
    response_headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/css; charset=utf-8"),
    );
    response_headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    // Cache intermediaries must key on the request's Accept-Encoding since
    // the body served for this same URL differs (plain/gzip/br).
    response_headers.insert(
        http::header::VARY,
        http::HeaderValue::from_static("Accept-Encoding"),
    );
    if let Some(encoding) = content_encoding {
        response_headers.insert(
            http::header::CONTENT_ENCODING,
            http::HeaderValue::from_static(encoding),
        );
    }

    // A weak validator: the identity/gzip/br byte streams served for this
    // one logical resource are not byte-identical, so a strong ETag (which
    // asserts byte-for-byte equivalence — see `ETag::strong`) would be
    // incorrect here, even though `Vary: Accept-Encoding` already keeps
    // cache entries for different encodings distinct.
    let etag = crate::etag::ETag::weak(body.into_etag().tag().to_owned());

    crate::etag::fresh_when(headers, etag)
        .or((response_headers, encoded_body))
        .into_response()
}

/// Serves the framework's default flash-message stylesheet
/// ([`crate::flash::FLASH_CSS`]) at [`crate::flash::FLASH_CSS_PATH`].
#[cfg(feature = "flash")]
pub async fn flash_css_handler(headers: http::HeaderMap) -> axum::response::Response {
    static PRECOMPRESSED: std::sync::OnceLock<PrecompressedCss> = std::sync::OnceLock::new();
    static_css_response(
        &headers,
        crate::flash::FLASH_CSS,
        PRECOMPRESSED.get_or_init(|| PrecompressedCss::compute(crate::flash::FLASH_CSS)),
    )
}

/// Serves the framework's widget stylesheet ([`crate::ui::WIDGETS_CSS`]) at
/// [`crate::ui::WIDGETS_CSS_PATH`] (#1215).
#[cfg(feature = "maud")]
pub async fn widgets_css_handler(headers: http::HeaderMap) -> axum::response::Response {
    static PRECOMPRESSED: std::sync::OnceLock<PrecompressedCss> = std::sync::OnceLock::new();
    static_css_response(
        &headers,
        crate::ui::WIDGETS_CSS,
        PRECOMPRESSED.get_or_init(|| PrecompressedCss::compute(crate::ui::WIDGETS_CSS)),
    )
}

#[cfg(feature = "htmx")]
pub async fn htmx_csrf_handler() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [
            (http::header::CONTENT_TYPE, "application/javascript"),
            (
                http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ),
        ],
        crate::htmx::HTMX_CSRF_JS,
    )
        .into_response()
}

#[cfg(feature = "htmx")]
pub async fn autumn_widgets_handler() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [
            (http::header::CONTENT_TYPE, "application/javascript"),
            (
                http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ),
        ],
        crate::htmx::AUTUMN_WIDGETS_JS,
    )
        .into_response()
}

/// Weak `ETag` for the vendored idiomorph script, derived once from the
/// embedded bytes.
///
/// The idiomorph URL is **not** content-fingerprinted, so it cannot safely use
/// an `immutable` cache. Instead the handler emits this content-derived `ETag`
/// alongside a revalidating `Cache-Control`, letting caches confirm freshness
/// (and pick up new bytes) whenever the vendored script changes.
///
/// The validator is **weak**: when compression is enabled, the response
/// compression layer gzips/brotli-encodes this `application/javascript`
/// response after the handler attaches the `ETag`, so the identity, gzip, and
/// br variants share one tag despite differing byte streams. A strong `ETag`
/// asserts byte-for-byte equivalence and would be invalid across those
/// encodings (matching the sibling CSS asset handler).
#[cfg(feature = "htmx")]
static IDIOMORPH_ETAG: std::sync::LazyLock<crate::etag::ETag> = std::sync::LazyLock::new(|| {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let digest = Sha256::digest(crate::htmx::IDIOMORPH_JS);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    crate::etag::ETag::weak(format!("idiomorph-{hex}"))
});

/// Serves the vendored idiomorph DOM-morphing library at [`crate::htmx::IDIOMORPH_JS_PATH`].
///
/// Idiomorph enables smooth DOM morphing via `hx-swap="morph"` in htmx.
///
/// Because the serving URL is not content-fingerprinted, the response uses a
/// revalidating cache policy (`must-revalidate` plus a weak content-derived
/// `ETag`) rather than a year-long `immutable` cache. This ensures clients that
/// cached an earlier version of the script pick up new bytes instead of running
/// a stale copy for up to a year.
#[cfg(feature = "htmx")]
pub async fn idiomorph_handler() -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut response = (
        [
            (http::header::CONTENT_TYPE, "application/javascript"),
            (
                http::header::CACHE_CONTROL,
                "public, max-age=0, must-revalidate",
            ),
        ],
        crate::htmx::IDIOMORPH_JS,
    )
        .into_response();
    response
        .headers_mut()
        .insert(http::header::ETAG, IDIOMORPH_ETAG.header_value());
    response
}

/// Serves the vendored htmx SSE extension at [`crate::htmx::HTMX_SSE_JS_PATH`].
///
/// The SSE extension enables `hx-ext="sse"` for server-sent event streams.
#[cfg(feature = "htmx")]
pub async fn htmx_sse_handler() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [
            (http::header::CONTENT_TYPE, "application/javascript"),
            (
                http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ),
        ],
        crate::htmx::HTMX_SSE_JS,
    )
        .into_response()
}

#[cfg(feature = "openapi")]
pub fn collect_openapi_docs(
    route_list: &[Route],
    scoped_groups: &[ScopedGroup],
) -> Vec<crate::openapi::ApiDoc> {
    // Walk both top-level routes and scoped groups. For scoped groups the
    // effective path is `prefix + route.path`; we materialize these into
    // fresh `ApiDoc`s so the rendered spec reflects the actual URL the
    // user will call.
    let mut docs: Vec<crate::openapi::ApiDoc> = Vec::new();
    for route in route_list {
        let mut doc = route.api_doc.clone();
        doc.api_version = route.api_version;
        doc.sunset_opt_out = route.sunset_opt_out;
        docs.push(doc);
    }
    for group in scoped_groups {
        // Extract `{name}` captures from the scope prefix so parameters
        // declared in the prefix (e.g. `/orgs/{org_id}`) show up on the
        // generated operation alongside the child route's own params.
        let prefix_params = extract_path_params(&group.prefix);
        for route in &group.routes {
            let mut doc = route.api_doc.clone();
            doc.api_version = route.api_version;
            doc.sunset_opt_out = route.sunset_opt_out;
            // Leak the combined path so it fits the `&'static str` shape of
            // ApiDoc. The spec is built once per process; the leak is
            // bounded by the route table size. Using the same
            // normalization as `join_nested_path` keeps the spec's
            // paths aligned with the URLs axum actually routes.
            let full = join_nested_path(&group.prefix, route.api_doc.path);
            doc.path = Box::leak(full.into_boxed_str());

            if !prefix_params.is_empty() {
                let mut merged: Vec<&'static str> = prefix_params
                    .iter()
                    .map(|p| &*Box::leak(p.clone().into_boxed_str()))
                    .collect();
                for existing in route.api_doc.path_params {
                    if !merged.iter().any(|n| n == existing) {
                        merged.push(existing);
                    }
                }
                doc.path_params = Box::leak(merged.into_boxed_slice());
            }

            docs.push(doc);
        }
    }
    docs
}

#[cfg(feature = "openapi")]
fn mount_swagger_ui_routes(
    mut router: axum::Router<AppState>,
    path: &str,
    title: &str,
    json_path: &str,
) -> axum::Router<AppState> {
    let [css_path, bundle_path, initializer_path] = crate::openapi::swagger_ui_asset_paths(path);
    let html_body = Arc::new(crate::openapi::swagger_ui_html(
        title,
        &css_path,
        &bundle_path,
        &initializer_path,
    ));
    let initializer_body = Arc::new(crate::openapi::swagger_ui_initializer_js(json_path));
    router = router.route(
        path,
        axum::routing::get(move || {
            let html = html_body.clone();
            async move {
                use axum::response::IntoResponse;
                (
                    [(http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    (*html).clone(),
                )
                    .into_response()
            }
        }),
    );
    router = router.route(
        &css_path,
        axum::routing::get(|| async move {
            use axum::response::IntoResponse;
            (
                [(http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
                crate::openapi::SWAGGER_UI_CSS,
            )
                .into_response()
        }),
    );
    router = router.route(
        &bundle_path,
        axum::routing::get(|| async move {
            use axum::body::Bytes;
            use axum::response::IntoResponse;
            (
                [(
                    http::header::CONTENT_TYPE,
                    "application/javascript; charset=utf-8",
                )],
                Bytes::from_static(crate::openapi::SWAGGER_UI_BUNDLE),
            )
                .into_response()
        }),
    );
    router = router.route(
        &initializer_path,
        axum::routing::get(move || {
            let js = initializer_body.clone();
            async move {
                use axum::response::IntoResponse;
                (
                    [(
                        http::header::CONTENT_TYPE,
                        "application/javascript; charset=utf-8",
                    )],
                    (*js).clone(),
                )
                    .into_response()
            }
        }),
    );
    router
}

/// Tower [`Layer`](tower::Layer) installing the app's registered
/// [`HttpInterceptor`](crate::interceptor::HttpInterceptor) as the ambient
/// interceptor chain for outbound `reqwest` calls made during this request.
///
/// Hand-rolled rather than an `axum::middleware::from_fn_with_state` for the
/// reason #2214 documents: `from_fn` `Box::pin`s the async block it generates
/// on every request. `tokio::task_local!`'s `scope` returns a named
/// `TaskLocalFuture`, so the scoped branch needs no box either — and an app
/// with no interceptor registered (the common case) forwards the inner
/// service's future completely untouched.
///
/// Like [`crate::events::EventAppContextLayer`], the inner future is built
/// inside a `sync_scope` as well as polled inside a `scope`, so the synchronous
/// `Service::call` chain beneath this layer also sees the interceptors.
#[cfg(feature = "oauth2")]
#[derive(Clone)]
pub struct HttpInterceptorLayer {
    state: AppState,
}

#[cfg(feature = "oauth2")]
impl HttpInterceptorLayer {
    pub(crate) const fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[cfg(feature = "oauth2")]
impl<S> tower::Layer<S> for HttpInterceptorLayer {
    type Service = HttpInterceptorService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        HttpInterceptorService {
            inner,
            state: self.state.clone(),
        }
    }
}

/// Tower [`Service`](tower::Service) produced by [`HttpInterceptorLayer`].
#[cfg(feature = "oauth2")]
#[derive(Clone)]
pub struct HttpInterceptorService<S> {
    inner: S,
    state: AppState,
}

#[cfg(feature = "oauth2")]
pin_project_lite::pin_project! {
    /// Future returned by [`HttpInterceptorService`].
    #[project = HttpInterceptorFutureProj]
    pub enum HttpInterceptorScopeFuture<F> {
        /// No interceptor registered: the inner service's own future.
        Plain {
            #[pin]
            inner: F,
        },
        /// The inner future, polled inside the interceptor task-local scope.
        Scoped {
            #[pin]
            inner: tokio::task::futures::TaskLocalFuture<
                Vec<Arc<dyn crate::interceptor::HttpInterceptor>>,
                F,
            >,
        },
    }
}

#[cfg(feature = "oauth2")]
impl<F: std::future::Future> std::future::Future for HttpInterceptorScopeFuture<F> {
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.project() {
            HttpInterceptorFutureProj::Plain { inner } => inner.poll(cx),
            HttpInterceptorFutureProj::Scoped { inner } => inner.poll(cx),
        }
    }
}

#[cfg(feature = "oauth2")]
impl<S, ReqBody> tower::Service<Request<ReqBody>> for HttpInterceptorService<S>
where
    S: tower::Service<Request<ReqBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = HttpInterceptorScopeFuture<S::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        use crate::interceptor::{ACTIVE_HTTP_INTERCEPTORS, HttpInterceptor};
        let Some(interceptor_arc) = self.state.extension::<Arc<dyn HttpInterceptor>>() else {
            return HttpInterceptorScopeFuture::Plain {
                inner: self.inner.call(req),
            };
        };
        let interceptors = vec![(*interceptor_arc).clone()];
        let inner =
            ACTIVE_HTTP_INTERCEPTORS.sync_scope(interceptors.clone(), || self.inner.call(req));
        HttpInterceptorScopeFuture::Scoped {
            inner: ACTIVE_HTTP_INTERCEPTORS.scope(interceptors, inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state() -> AppState {
        AppState {
            profile: Some("test".into()),
            ..AppState::test_default()
        }
    }

    // ── submit-token production memory guard wiring (Finding O) ─────────────

    #[test]
    fn submit_token_explicit_memory_in_production_fails_router_build() {
        // EXPLICIT `[security.submit_token].backend = "memory"` + production →
        // hard fail at router build, mirroring the idempotency prod-memory guard.
        let mut config = AutumnConfig::default();
        config.security.submit_token.backend = Some(crate::config::IdempotencyBackend::Memory);
        let err = apply_submit_token_middleware(axum::Router::<()>::new(), &config, true)
            .expect_err("explicit memory submit-token backend in prod must fail router build");
        assert!(
            matches!(err, RouterBuildError::InvalidSubmitTokenBackend(_)),
            "expected InvalidSubmitTokenBackend, got {err:?}"
        );
    }

    #[test]
    fn submit_token_inherited_memory_in_production_builds() {
        // INHERITED default (`backend = None`) resolving to Memory in prod must
        // NOT fail — it only warns. Router build succeeds.
        let mut config = AutumnConfig::default();
        config.security.submit_token.backend = None;
        config.idempotency.backend = crate::config::IdempotencyBackend::Memory;
        let _router = apply_submit_token_middleware(axum::Router::<()>::new(), &config, true)
            .expect("inherited memory submit-token backend in prod must still build (warn only)");
    }

    #[test]
    fn submit_token_memory_outside_production_builds() {
        // Non-production → no fail regardless of explicit memory.
        let mut config = AutumnConfig::default();
        config.security.submit_token.backend = Some(crate::config::IdempotencyBackend::Memory);
        let _router = apply_submit_token_middleware(axum::Router::<()>::new(), &config, false)
            .expect("memory submit-token backend outside production must build");
    }

    #[tokio::test]
    async fn build_router_mounts_actuator_at_configured_prefix() {
        let mut config = AutumnConfig::default();
        config.actuator.prefix = "/ops".to_owned();
        config.actuator.sensitive = true;

        let app = build_router(Vec::new(), &config, test_state());

        let prefixed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ops/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(prefixed.status(), StatusCode::OK);

        let legacy = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy.status(), StatusCode::NOT_FOUND);
    }

    /// Worker-role (#1613) probe-only router: exposes the framework probes and
    /// the actuator, but no user routes. A sample user path 404s.
    #[tokio::test]
    async fn probe_only_router_mounts_probes_and_actuator_but_no_user_routes() {
        let config = AutumnConfig::default();
        let app = try_build_probe_only_router(&config, test_state())
            .expect("probe-only router should build");

        // Probe + actuator paths respond.
        for path in [
            config.health.live_path.as_str(),
            config.health.ready_path.as_str(),
            config.health.startup_path.as_str(),
            config.health.path.as_str(),
            "/actuator/health",
            "/actuator/info",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_ne!(
                response.status(),
                StatusCode::NOT_FOUND,
                "probe-only router should serve {path}"
            );
        }

        // A made-up user route is absent (probe-only router has no user table).
        let missing = app
            .oneshot(
                Request::builder()
                    .uri("/definitely-not-a-user-route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    /// Issue #1971: a user route registered at the auto-mounted health path
    /// must WIN. The router build succeeds — no raw axum "Overlapping method
    /// route. Handler for GET /health already exists" panic — the user's own
    /// handler serves `/health`, and the remaining built-in probes (`/live`,
    /// `/ready`, `/startup`) still mount and respond.
    #[tokio::test]
    async fn user_route_at_health_path_overrides_builtin_probe() {
        async fn user_health() -> &'static str {
            "user-health-handler"
        }

        let config = AutumnConfig::default();
        // Precondition: the default health alias is exactly the path we shadow.
        assert_eq!(config.health.path, "/health");

        let route = Route {
            method: http::Method::GET,
            path: "/health",
            handler: axum::routing::get(user_health),
            name: "user_health",
            api_doc: crate::openapi::ApiDoc {
                method: "GET",
                path: "/health",
                operation_id: "user_health",
                success_status: 200,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        };

        // Before the fix this panicked inside `axum::Router::route`; now it
        // builds cleanly (`build_router` panics on any RouterBuildError, so a
        // successful return also proves no structured error is raised).
        let app = build_router(vec![route], &config, test_state());

        // `/health` is served by the USER handler, not the framework probe.
        let response = app
            .clone()
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
        assert_eq!(
            &body[..],
            b"user-health-handler",
            "user route must win at the health path"
        );

        // The other built-in probes are untouched and still respond.
        for path in ["/live", "/ready", "/startup"] {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "built-in probe {path} should still be mounted"
            );
        }
    }

    /// Issue #1971: `health.enabled = false` is a global off-switch — the
    /// framework auto-mounts NONE of the built-in probes (health/live/ready/
    /// startup). The router still builds cleanly (`build_router` panics on any
    /// `RouterBuildError`, so a successful return proves no structured error is
    /// raised), and every probe path resolves to `404` because nothing owns it.
    #[tokio::test]
    async fn health_enabled_false_mounts_no_builtin_probes() {
        let mut config = AutumnConfig::default();
        config.health.enabled = false;

        let app = build_router(vec![], &config, test_state());

        for path in [
            config.health.path.as_str(),
            config.health.live_path.as_str(),
            config.health.ready_path.as_str(),
            config.health.startup_path.as_str(),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "probe path {path} must not be auto-mounted when health.enabled = false"
            );
        }
    }

    /// The framework-owned widget stylesheet (#1215) is served the same way
    /// as the flash stylesheet: a same-origin, immutably-cached asset — not
    /// inline styles — so a strict `style-src 'self'` CSP still works and the
    /// asset is embeddable in the single binary (#1004) with no loose files.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn widgets_css_route_serves_the_shared_stylesheet() {
        let app = build_router(Vec::new(), &AutumnConfig::default(), test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri(crate::ui::WIDGETS_CSS_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(content_type.contains("text/css"), "{content_type}");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains(".autumn-field"), "{body}");
        assert!(body.contains(":root"), "{body}");
    }

    /// The widget stylesheet is conditional-GET aware (shared `static_css_response`
    /// helper): a revalidating client sends back the `ETag` it was given and gets
    /// a bodyless `304`, instead of re-downloading the full asset every time the
    /// far-future `Cache-Control` gets bypassed (hard refresh, a CDN stripping
    /// cache headers, etc.).
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn widgets_css_route_supports_conditional_get() {
        let app = build_router(Vec::new(), &AutumnConfig::default(), test_state());

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(crate::ui::WIDGETS_CSS_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let etag = first
            .headers()
            .get(http::header::ETAG)
            .expect("widget stylesheet response should carry an ETag")
            .clone();

        let revalidated = app
            .oneshot(
                Request::builder()
                    .uri(crate::ui::WIDGETS_CSS_PATH)
                    .header(http::header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
        let revalidated_body = axum::body::to_bytes(revalidated.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(revalidated_body.is_empty());
    }

    /// The widget stylesheet's `ETag` must be weak (`W/"..."`), not strong:
    /// the identity/gzip/br byte streams served under it are not
    /// byte-identical, and a strong `ETag` asserts exactly that (RFC 7232
    /// §2.1).
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn widgets_css_route_etag_is_weak_not_strong() {
        let app = build_router(Vec::new(), &AutumnConfig::default(), test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri(crate::ui::WIDGETS_CSS_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let etag = response
            .headers()
            .get(http::header::ETAG)
            .expect("widget stylesheet response should carry an ETag")
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            etag.starts_with("W/\""),
            "ETag must be weak since encoded variants aren't byte-identical: {etag}"
        );
    }

    /// A client that sends `Accept-Encoding: br` gets the pre-computed brotli
    /// encoding straight back (`Content-Encoding: br`), not a plain body that
    /// the outer `CompressionLayer` then has to compress on the fly.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn widgets_css_route_serves_precompressed_brotli_when_accepted() {
        let app = build_router(Vec::new(), &AutumnConfig::default(), test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri(crate::ui::WIDGETS_CSS_PATH)
                    .header(http::header::ACCEPT_ENCODING, "br")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .unwrap(),
            "br"
        );
        assert_eq!(
            response.headers().get(http::header::VARY).unwrap(),
            "Accept-Encoding"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let mut decoded = Vec::new();
        brotli::BrotliDecompress(&mut std::io::Cursor::new(body.as_ref()), &mut decoded)
            .expect("response body must be valid brotli");
        assert_eq!(String::from_utf8(decoded).unwrap(), crate::ui::WIDGETS_CSS);
    }

    /// Same as the brotli case, for a `gzip`-only client.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn widgets_css_route_serves_precompressed_gzip_when_accepted() {
        let app = build_router(Vec::new(), &AutumnConfig::default(), test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri(crate::ui::WIDGETS_CSS_PATH)
                    .header(http::header::ACCEPT_ENCODING, "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .unwrap(),
            "gzip"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let mut gz = flate2::read::GzDecoder::new(body.as_ref());
        let mut output = String::new();
        std::io::Read::read_to_string(&mut gz, &mut output)
            .expect("response body must be valid gzip");
        assert_eq!(output, crate::ui::WIDGETS_CSS);
    }

    /// `q=0` is an explicit opt-out (RFC 7231 §5.3.4): a client that lists
    /// `br` but disqualifies it must fall back to the identity encoding
    /// rather than being served brotli anyway.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn widgets_css_route_honors_q_zero_opt_out() {
        let app = build_router(Vec::new(), &AutumnConfig::default(), test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri(crate::ui::WIDGETS_CSS_PATH)
                    .header(http::header::ACCEPT_ENCODING, "br;q=0, gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .unwrap(),
            "gzip"
        );
    }

    /// No `Accept-Encoding` header at all means identity — no
    /// `Content-Encoding` header, plain-text body (matches the existing
    /// `widgets_css_route_serves_the_shared_stylesheet` assertions).
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn widgets_css_route_serves_identity_with_no_accept_encoding() {
        let app = build_router(Vec::new(), &AutumnConfig::default(), test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri(crate::ui::WIDGETS_CSS_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response
                .headers()
                .contains_key(http::header::CONTENT_ENCODING)
        );
    }

    /// Pins the production access-log wiring (#999): the layer is applied in
    /// `apply_startup_barrier`, outside the barrier itself, so even requests
    /// rejected with 503 before the app router runs emit one access event
    /// carrying the status the client receives.
    #[test]
    fn startup_barrier_503s_are_access_logged() {
        use tracing_subscriber::layer::SubscriberExt as _;

        #[derive(Clone, Default)]
        struct Capture {
            events: Arc<std::sync::Mutex<Vec<std::collections::BTreeMap<String, String>>>>,
        }
        struct Visitor<'a>(&'a mut std::collections::BTreeMap<String, String>);
        impl tracing::field::Visit for Visitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.insert(field.name().to_owned(), format!("{value:?}"));
            }
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.insert(field.name().to_owned(), value.to_string());
            }
        }
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if event.metadata().target() != crate::middleware::ACCESS_LOG_TARGET {
                    return;
                }
                let mut fields = std::collections::BTreeMap::new();
                event.record(&mut Visitor(&mut fields));
                self.events.lock().unwrap().push(fields);
            }
        }

        let capture = Capture::default();
        let events = Arc::clone(&capture.events);
        let subscriber = tracing_subscriber::registry().with(capture);

        tracing::subscriber::with_default(subscriber, || {
            // With startup incomplete, the barrier rejects non-probe requests
            // with 503 before the app router runs.
            let state = AppState::for_test()
                .with_profile("test")
                .with_startup_complete(false);
            let app = build_router(Vec::new(), &AutumnConfig::default(), state);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            // A `tracing` callsite `Interest` is one value cached per callsite
            // for the whole process, combined from every active dispatcher.
            // `cargo test` runs this alongside thousands of unit tests in the
            // same binary, many touching the `autumn::access` callsite with no
            // capturing subscriber, so the combined interest can be re-cached as
            // "not interested" in the narrow window between rebuilding it and
            // firing the request below. Rebuilding and re-firing converges almost
            // immediately, so retry a few times rather than flake.
            let mut response = None;
            for attempt in 1..=5 {
                tracing::callsite::rebuild_interest_cache();
                let resp = rt.block_on(async {
                    app.clone()
                        .oneshot(
                            Request::builder()
                                .uri("/not-a-probe")
                                .body(Body::empty())
                                .unwrap(),
                        )
                        .await
                        .unwrap()
                });
                let captured = !events.lock().unwrap().is_empty();
                response = Some(resp);
                if captured {
                    break;
                }
                assert!(
                    attempt < 5,
                    "access-log event was not captured after {attempt} attempts \
                     (tracing interest-cache race with a concurrent test)"
                );
            }
            assert_eq!(response.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
        });

        let events = events.lock().unwrap().clone();
        assert_eq!(
            events.len(),
            1,
            "a barrier-rejected request should emit one access event: {events:?}"
        );
        assert_eq!(events[0].get("status").map(String::as_str), Some("503"));
        assert!(
            !events[0].contains_key("request_id"),
            "barrier short-circuits before RequestIdLayer, so no request id"
        );
    }

    /// Pins the Server-Timing fallback wiring (#1348): the fallback layer is
    /// applied in `apply_startup_barrier`, outside the barrier itself, so a
    /// request rejected with 503 before the app router (and its primary
    /// `ServerTimingLayer`) runs still carries a `Server-Timing` header. Without
    /// the fallback the header is silently dropped on these short-circuits.
    #[tokio::test]
    async fn startup_barrier_503s_carry_server_timing_header() {
        // Startup incomplete → the barrier 503s non-probe requests before the
        // app router (and the primary ServerTimingLayer) ever run.
        let state = AppState::for_test()
            .with_profile("test")
            .with_startup_complete(false);
        let mut config = AutumnConfig::default();
        config.observability.server_timing = Some(true);

        let app = build_router(Vec::new(), &config, state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/not-a-probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let header = response
            .headers()
            .get("server-timing")
            .expect("startup 503 short-circuit should still carry Server-Timing via the fallback")
            .to_str()
            .expect("server-timing header should be valid ASCII");
        assert!(
            header.starts_with("total;dur="),
            "fallback should emit a `total` metric, got {header:?}"
        );
        // Exactly one metric on the short-circuit path — the primary never ran,
        // so there is no second `total`.
        assert_eq!(
            header.matches("total;dur=").count(),
            1,
            "short-circuit response must carry a single total metric: {header:?}"
        );
    }

    #[test]
    fn try_build_router_rejects_invalid_session_backend_config() {
        let mut config = AutumnConfig::default();
        config.session.backend = crate::session::SessionBackend::Redis;

        let error = try_build_router(Vec::new(), &config, test_state())
            .expect_err("missing redis config should fail checked router build");

        assert!(matches!(
            error,
            RouterBuildError::InvalidSessionBackend(
                crate::session::SessionBackendConfigError::MissingRedisUrl
            )
        ));
    }

    #[test]
    fn try_build_router_with_static_rejects_invalid_session_backend_config() {
        let mut config = AutumnConfig::default();
        config.session.backend = crate::session::SessionBackend::Redis;

        let error = try_build_router_with_static(Vec::new(), &config, test_state(), None)
            .expect_err("missing redis config should fail checked static router build");

        assert!(matches!(
            error,
            RouterBuildError::InvalidSessionBackend(
                crate::session::SessionBackendConfigError::MissingRedisUrl
            )
        ));
    }

    #[test]
    fn try_build_router_returns_error_for_probe_actuator_path_overlap() {
        let mut config = AutumnConfig::default();
        config.actuator.prefix = "/".to_owned();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            try_build_router(Vec::new(), &config, test_state())
        }));

        assert!(result.is_ok(), "try_build_router panicked on route overlap");
        assert!(
            result.unwrap().is_err(),
            "route overlap should be reported as a checked router build error"
        );
    }

    /// Regression for issue #1971 P2: when a user route already owns `/health`,
    /// the built-in probe cedes that path (#1971) — but a root-prefix actuator
    /// still normalizes its own `GET /health` onto it. The ceded probe path must
    /// remain visible to the actuator overlap guard so this surfaces as a checked
    /// `FrameworkRouteOverlap` rather than an axum construction panic (matching
    /// the no-user-route case in
    /// `try_build_router_returns_error_for_probe_actuator_path_overlap`).
    #[test]
    fn probe_actuator_overlap_detected_when_user_route_owns_probe_path() {
        async fn user_health() -> &'static str {
            "user-health-handler"
        }

        let mut config = AutumnConfig::default();
        config.actuator.prefix = "/".to_owned();
        // Precondition: the user route, the ceded probe, and the actuator all
        // land on exactly `/health`.
        assert_eq!(config.health.path, "/health");

        let route = Route {
            method: http::Method::GET,
            path: "/health",
            handler: axum::routing::get(user_health),
            name: "user_health",
            api_doc: crate::openapi::ApiDoc {
                method: "GET",
                path: "/health",
                operation_id: "user_health",
                success_status: 200,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            try_build_router(vec![route], &config, test_state())
        }));

        assert!(
            result.is_ok(),
            "try_build_router panicked instead of returning a checked overlap error"
        );
        let build = result.unwrap();
        assert!(
            matches!(
                &build,
                Err(RouterBuildError::FrameworkRouteOverlap {
                    path,
                    incoming: "actuator endpoint",
                    ..
                }) if path == "/health"
            ),
            "root-prefix actuator over a user-owned probe path must yield a checked \
             FrameworkRouteOverlap for /health, got: {:?}",
            build.as_ref().map(|_| "Ok(router)"),
        );
    }

    #[tokio::test]
    async fn apply_cors_middleware_skipped_when_no_origins() {
        let config = AutumnConfig::default();
        assert!(config.cors.allowed_origins.is_empty());

        let base: axum::Router<AppState> =
            axum::Router::new().route("/test", axum::routing::get(|| async { "ok" }));
        let router = apply_cors_middleware(base, &config).with_state(test_state());

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
                .is_none(),
            "CORS header must be absent when no origins are configured"
        );
    }

    #[tokio::test]
    async fn apply_cors_middleware_present_when_origins_configured() {
        let mut config = AutumnConfig::default();
        config.cors.allowed_origins = vec!["https://example.com".to_owned()];

        let base: axum::Router<AppState> =
            axum::Router::new().route("/test", axum::routing::get(|| async { "ok" }));
        let router = apply_cors_middleware(base, &config).with_state(test_state());

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
                .is_some(),
            "CORS header must be present when origins are configured"
        );
    }

    #[tokio::test]
    async fn apply_cors_middleware_handles_preflight_request() {
        let mut config = AutumnConfig::default();
        config.cors.allowed_origins = vec!["https://example.com".to_owned()];

        let base: axum::Router<AppState> =
            axum::Router::new().route("/api/widgets", axum::routing::post(|| async { "ok" }));
        let router = apply_cors_middleware(base, &config).with_state(test_state());

        let response = router
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/api/widgets")
                    .header("Origin", "https://example.com")
                    .header("Access-Control-Request-Method", "POST")
                    .header("Access-Control-Request-Headers", "Content-Type")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let headers = response.headers();
        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("https://example.com"),
            "preflight must echo the allowed origin"
        );
        assert!(
            headers.get("access-control-allow-methods").is_some(),
            "preflight must advertise allowed methods"
        );
        assert!(
            headers.get("access-control-allow-headers").is_some(),
            "preflight must advertise allowed headers"
        );
        assert!(
            headers.get("access-control-max-age").is_some(),
            "preflight must advertise max-age so browsers can cache it"
        );
    }

    #[tokio::test]
    async fn apply_csrf_middleware_skipped_when_disabled() {
        let config = AutumnConfig::default();
        assert!(!config.security.csrf.enabled);

        let base: axum::Router<AppState> =
            axum::Router::new().route("/form", axum::routing::post(|| async { "posted" }));
        let router = apply_csrf_middleware(base, &config, None).with_state(test_state());

        // Without CSRF the POST should pass through with no CSRF-specific response
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/form")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn apply_rate_limit_middleware_skipped_when_disabled() {
        let config = AutumnConfig::default();
        assert!(!config.security.rate_limit.enabled);

        let base: axum::Router<AppState> =
            axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
        let state = test_state();
        let router = apply_rate_limit_middleware(base, &config, &state).with_state(state.clone());

        // Fire several rapid requests; none should be throttled.
        for _ in 0..5 {
            let response = router
                .clone()
                .oneshot(Request::builder().uri("/ping").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn apply_rate_limit_middleware_returns_429_when_exhausted() {
        let mut config = AutumnConfig::default();
        config.security.rate_limit.enabled = true;
        config.security.rate_limit.requests_per_second = 0.1;
        config.security.rate_limit.burst = 1;
        config.security.rate_limit.trust_forwarded_headers = true;

        let base: axum::Router<AppState> =
            axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
        let state = test_state();
        let router = apply_rate_limit_middleware(base, &config, &state).with_state(state.clone());

        let ok = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ping")
                    .header("X-Forwarded-For", "203.0.113.9")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);

        let blocked = router
            .oneshot(
                Request::builder()
                    .uri("/ping")
                    .header("X-Forwarded-For", "203.0.113.9")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(blocked.headers().get("retry-after").is_some());
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn mcp_envelope_is_gated_during_maintenance() {
        use crate::maintenance::{MaintenanceConfig, MaintenanceState};

        // Trust the host the control request sends so that, with maintenance
        // off, the envelope's host guard lets `initialize` through.
        let mut config = AutumnConfig::default();
        config.security.trusted_hosts.hosts = vec!["app.example".to_owned()];

        let wiring = crate::mcp::McpWiring {
            cors: crate::config::CorsConfig::default(),
            trusted_hosts: TrustedHostPolicy::from_config(&config),
            tenant_header: None,
            csrf_header: "x-csrf-token".to_owned(),
            envelope_rate_limited: false,
            envelope_load_shed: false,
            state: test_state(),
        };
        let mcp_router =
            crate::mcp::build_mcp_router("/mcp", Vec::new(), axum::Router::new(), wiring, None);

        let initialize = || {
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "app.example")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize"}).to_string(),
                ))
                .unwrap()
        };

        // Maintenance ON: the late-mounted envelope returns the documented 503
        // instead of serving the catalog — the gap this layer closes.
        let state = test_state();
        let maintenance = MaintenanceState::new();
        maintenance.enable(MaintenanceConfig::default());
        state.insert_extension(maintenance);
        let gated = mcp_router
            .clone()
            .layer(build_maintenance_layer(&config, &state))
            .with_state(state);
        let resp = gated.oneshot(initialize()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        // Maintenance OFF (no enabled state): the same envelope serves
        // `initialize` normally, confirming the gate is the only difference.
        let state = test_state();
        let open = mcp_router
            .layer(build_maintenance_layer(&config, &state))
            .with_state(state);
        let resp = open.oneshot(initialize()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[cfg(feature = "mail")]
    fn dev_mail_preview_config(dir: &std::path::Path) -> AutumnConfig {
        let mut config = AutumnConfig {
            profile: Some("dev".to_owned()),
            mail: crate::mail::MailConfig {
                transport: crate::mail::Transport::File,
                file_dir: dir.to_path_buf(),
                ..Default::default()
            },
            ..Default::default()
        };
        config.security.trusted_hosts.hosts = vec!["example.com".to_owned()];
        config
    }

    #[cfg(any(feature = "mail", feature = "maud"))]
    async fn response_text(response: axum::response::Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should collect");
        String::from_utf8(body.to_vec()).expect("body should be utf8")
    }

    #[cfg(feature = "mail")]
    #[tokio::test]
    async fn build_router_mounts_dev_mail_preview_empty_state_for_file_transport() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dev_mail_preview_config(dir.path());
        let router = build_router(Vec::new(), &config, test_state());

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/_autumn/mail")
                    .header("host", "example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_text(response).await;
        assert!(
            body.contains("No captured emails"),
            "missing empty state: {body}"
        );
        assert!(
            body.contains("mail.transport = &quot;file&quot;"),
            "empty state should explain capture setup: {body}"
        );
    }

    #[cfg(feature = "mail")]
    #[tokio::test]
    async fn build_router_lists_captured_mail_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let older = dir.path().join("older.eml");
        let newer = dir.path().join("newer.eml");
        std::fs::write(
            &older,
            "To: first@example.com\nSubject: First\nDate: Tue, 05 May 2026 10:00:00 +0000\nMessage-Id: <first@example.com>\n\nfirst body\n",
        )
        .expect("write older eml");
        std::fs::write(
            &newer,
            "To: second@example.com\nSubject: Second\nDate: Tue, 05 May 2026 10:01:00 +0000\nMessage-Id: <second@example.com>\n\nsecond body\n",
        )
        .expect("write newer eml");
        filetime::set_file_mtime(&older, filetime::FileTime::from_unix_time(100, 0))
            .expect("set older mtime");
        filetime::set_file_mtime(&newer, filetime::FileTime::from_unix_time(200, 0))
            .expect("set newer mtime");

        let config = dev_mail_preview_config(dir.path());
        let router = build_router(Vec::new(), &config, test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/_autumn/mail")
                    .header("host", "example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_text(response).await;
        let second = body.find("Second").expect("newer subject should render");
        let first = body.find("First").expect("older subject should render");
        assert!(second < first, "newest message should render first: {body}");
        assert!(
            body.contains("second@example.com"),
            "missing To column: {body}"
        );
        assert!(
            body.contains("Timestamp"),
            "missing timestamp column: {body}"
        );
    }

    #[cfg(feature = "mail")]
    #[tokio::test]
    async fn build_router_mail_preview_detail_renders_html_in_sandboxed_iframe() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("detail.eml"),
            "From: Autumn <noreply@example.com>\nTo: ada@example.com\nReply-To: support@example.com\nSubject: Reset\nDate: Tue, 05 May 2026 10:00:00 +0000\nMessage-Id: <reset@example.com>\nMIME-Version: 1.0\nContent-Type: multipart/alternative; boundary=\"autumn-mail\"\n\n--autumn-mail\nContent-Type: text/plain; charset=utf-8\n\nPlain reset\n--autumn-mail\nContent-Type: text/html; charset=utf-8\n\n<h1>Hello iframe</h1>\n--autumn-mail--\n",
        )
        .expect("write detail eml");

        let config = dev_mail_preview_config(dir.path());
        let router = build_router(Vec::new(), &config, test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/_autumn/mail/messages/detail.eml")
                    .header("host", "example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_text(response).await;
        assert!(body.contains("<iframe"), "missing iframe: {body}");
        assert!(body.contains("sandbox"), "iframe must be sandboxed: {body}");
        assert!(body.contains("Hello iframe"), "missing html body: {body}");
        assert!(body.contains("Plain text"), "missing text toggle: {body}");
        assert!(body.contains("Headers"), "missing headers toggle: {body}");
        assert!(
            body.contains("Raw .eml"),
            "missing raw source toggle: {body}"
        );
        assert!(
            body.contains("Message-Id"),
            "missing message id header: {body}"
        );
    }

    #[cfg(feature = "mail")]
    #[tokio::test]
    async fn build_router_does_not_mount_mail_preview_outside_dev() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = dev_mail_preview_config(dir.path());
        config.profile = Some("prod".to_owned());
        let router = build_router(Vec::new(), &config, test_state());

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/_autumn/mail")
                    .header("host", "example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ── Widget story gallery mount gating (issue #1526) ─────────────────────
    //
    // Unlike the dev-only mail preview, `/_stories` is opt-in in ANY profile
    // via `[stories] enabled = true` (default false): mounting is gated only
    // on the resolved config flag, while handlers read the `StoryRegistry`
    // from the AppState extension installed by `with_story_gallery`.

    #[cfg(feature = "maud")]
    fn story_gallery_config() -> AutumnConfig {
        let mut config = AutumnConfig::default();
        config.stories.enabled = true;
        config.security.trusted_hosts.hosts = vec!["example.com".to_owned()];
        config
    }

    #[cfg(feature = "maud")]
    fn stories_state_with_builtin() -> AppState {
        let state = test_state();
        state.insert_extension(crate::stories::builtin());
        state
    }

    #[cfg(feature = "maud")]
    async fn get_with_host(router: axum::Router, uri: &str) -> axum::response::Response {
        router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("host", "example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// T1 (AC4/AC5): with `[stories] enabled = true` and the builtin registry
    /// installed, the grouped index is served at `/_stories` and pulls in the
    /// framework widget stylesheet.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn build_router_mounts_story_gallery_when_enabled() {
        let router = build_router(
            Vec::new(),
            &story_gallery_config(),
            stories_state_with_builtin(),
        );

        let response = get_with_host(router, crate::stories::STORIES_PATH).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_text(response).await;
        assert!(
            body.contains("Data table"),
            "index should list builtin story names: {body}"
        );
        assert!(
            body.contains("autumn-widgets.css"),
            "index should link the framework widget stylesheet: {body}"
        );
    }

    /// T2 (AC4): the detail route serves the live render plus Source and
    /// Rendered HTML tabs.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn story_detail_route_serves_render_source_and_html() {
        let router = build_router(
            Vec::new(),
            &story_gallery_config(),
            stories_state_with_builtin(),
        );

        let response = get_with_host(router, "/_stories/data-table").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_text(response).await;
        assert!(
            body.contains("<table"),
            "detail page must contain the live data_table render: {body}"
        );
        assert!(
            body.contains("data_table("),
            "detail page must show the source snippet that produced the render: {body}"
        );
        assert!(
            body.contains("Rendered HTML"),
            "detail page must offer the rendered-HTML tab: {body}"
        );
        assert!(
            body.contains("Source"),
            "detail page must offer the source tab: {body}"
        );
    }

    /// T3 (AC4): a mounted gallery 404s unknown slugs while the index stays up.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn story_detail_unknown_slug_is_404() {
        let router = build_router(
            Vec::new(),
            &story_gallery_config(),
            stories_state_with_builtin(),
        );

        let missing = get_with_host(router.clone(), "/_stories/nope").await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let index = get_with_host(router, "/_stories").await;
        assert_eq!(
            index.status(),
            StatusCode::OK,
            "index route must exist even when a slug misses"
        );
    }

    /// T4 (AC5/AC6): off by default — no `[stories] enabled = true`, no
    /// routes, even when a registry extension is installed.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn build_router_omits_story_gallery_by_default() {
        let mut config = AutumnConfig::default();
        assert!(
            !config.stories.enabled,
            "stories gallery must be off by default"
        );
        config.security.trusted_hosts.hosts = vec!["example.com".to_owned()];

        let router = build_router(Vec::new(), &config, stories_state_with_builtin());
        let response = get_with_host(router, "/_stories").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ── Locale-prefixed routing (issue #1251) ───────────────────────────────

    #[cfg(feature = "i18n")]
    mod locale_prefix_routing {
        use super::*;
        use crate::i18n::{Bundle, I18nConfig, Locale};

        async fn locale_probe(locale: Locale) -> String {
            locale.tag().to_owned()
        }

        async fn plain_ok() -> &'static str {
            "ok"
        }

        fn simple_route(path: &'static str, name: &'static str, probe: bool) -> Route {
            Route {
                method: http::Method::GET,
                path,
                handler: if probe {
                    axum::routing::get(locale_probe)
                } else {
                    axum::routing::get(plain_ok)
                },
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

        fn config(supported: &[&str], exclude: &[&str]) -> AutumnConfig {
            let mut config = AutumnConfig::default();
            config.i18n.locale_prefix_enabled = true;
            config.i18n.default_locale = supported.first().copied().unwrap_or("en").to_owned();
            config.i18n.supported_locales = supported.iter().map(|s| (*s).to_owned()).collect();
            config.i18n.locale_prefix_exclude = exclude.iter().map(|s| (*s).to_owned()).collect();
            config
        }

        fn bundle(supported: &[&str]) -> Arc<Bundle> {
            let cfg = I18nConfig {
                default_locale: supported.first().copied().unwrap_or("en").to_owned(),
                supported_locales: supported.iter().map(|s| (*s).to_owned()).collect(),
                fallback_chain: vec![],
                dir: "i18n".to_owned(),
                locale_prefix_enabled: false,
                locale_prefix_exclude: vec![],
                locale_prefix_exclude_exact: vec![],
            };
            Arc::new(Bundle::from_messages(
                std::collections::HashMap::new(),
                &cfg,
            ))
        }

        async fn request(
            router: &axum::Router,
            uri: &str,
            headers: &[(&str, &str)],
        ) -> axum::response::Response {
            request_method(router, http::Method::GET, uri, headers).await
        }

        async fn request_method(
            router: &axum::Router,
            method: http::Method,
            uri: &str,
            headers: &[(&str, &str)],
        ) -> axum::response::Response {
            let mut req = Request::builder().method(method).uri(uri);
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            router
                .clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap()
        }

        async fn body_string(resp: axum::response::Response) -> String {
            let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        }

        async fn uri_probe(uri: axum::http::Uri) -> String {
            uri.path().to_owned()
        }

        async fn slug_probe(
            locale: Locale,
            axum::extract::Path(slug): axum::extract::Path<String>,
        ) -> String {
            format!("{}:{slug}", locale.tag())
        }

        fn uri_probe_route(path: &'static str, name: &'static str) -> Route {
            Route {
                method: http::Method::GET,
                path,
                handler: axum::routing::get(uri_probe),
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

        fn slug_route(path: &'static str, name: &'static str) -> Route {
            Route {
                method: http::Method::GET,
                path,
                handler: axum::routing::get(slug_probe),
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

        fn make_post_route(path: &'static str, name: &'static str) -> Route {
            Route {
                method: http::Method::POST,
                path,
                handler: axum::routing::post(plain_ok),
                name,
                api_doc: crate::openapi::ApiDoc {
                    method: "POST",
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

        /// Codex review (P1): an app that enables `locale_prefix_enabled`
        /// without also calling `.i18n()`/`.i18n_auto()` (no `Bundle`
        /// installed) must still redirect to — and correctly serve — its
        /// *configured* locale, not a hard-coded `"en"` that may not even be
        /// in `supported_locales`.
        #[tokio::test]
        async fn locale_prefix_redirect_works_without_an_i18n_bundle() {
            let config = config(&["fr"], &[]);
            let route = simple_route("/posts", "posts", true);
            // No `.layer(axum::Extension(bundle(...)))` — deliberately no
            // Bundle, unlike every other test in this module.
            let app = build_router(vec![route], &config, test_state());

            let redirected = request(&app, "/posts", &[]).await;
            assert_eq!(redirected.status(), StatusCode::PERMANENT_REDIRECT);
            assert_eq!(
                redirected
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/fr/posts"),
                "must redirect to the configured locale, not a hard-coded \"en\""
            );

            let target = request(&app, "/fr/posts", &[]).await;
            assert_eq!(
                target.status(),
                StatusCode::OK,
                "the redirect target must actually resolve"
            );
            assert_eq!(body_string(target).await, "fr");
        }

        /// AC: default off — no behavior change for existing apps. The bare
        /// route serves directly; no locale nest exists.
        #[tokio::test]
        async fn locale_prefix_routing_off_by_default() {
            let mut config = AutumnConfig::default();
            assert!(!config.i18n.locale_prefix_enabled);
            config.i18n.supported_locales = vec!["en".to_owned(), "es".to_owned()];

            let route = simple_route("/posts", "posts", false);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let bare = request(&app, "/posts", &[]).await;
            assert_eq!(bare.status(), StatusCode::OK);

            let prefixed = request(&app, "/en/posts", &[]).await;
            assert_eq!(
                prefixed.status(),
                StatusCode::NOT_FOUND,
                "no locale nest should exist when the flag is off"
            );
        }

        /// The root path (`/`) is axum's nest-plus-root special case: `nest(
        /// "/en", router_with_route_at("/"))` makes bare `/en` (no trailing
        /// slash) match, while `/en/` 404s — the opposite of every other
        /// path, where the locale segment is a plain concatenation. Both the
        /// redirect target and direct nested access must account for this.
        #[tokio::test]
        async fn root_path_redirects_to_bare_locale_prefix_without_trailing_slash() {
            let config = config(&["en", "es"], &[]);
            let route = simple_route("/", "root", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let bare = request(&app, "/", &[]).await;
            assert_eq!(bare.status(), StatusCode::PERMANENT_REDIRECT);
            assert_eq!(
                bare.headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/en"),
                "root redirect target must have no trailing slash"
            );

            let nested = request(&app, "/en", &[]).await;
            assert_eq!(nested.status(), StatusCode::OK);
            assert_eq!(body_string(nested).await, "en");

            let nested_with_slash = request(&app, "/en/", &[]).await;
            assert_eq!(
                nested_with_slash.status(),
                StatusCode::NOT_FOUND,
                "axum's nest-plus-root case does not also match the trailing-slash form"
            );
        }

        /// The root path with a query string preserves the query and still
        /// omits the trailing slash before it.
        #[tokio::test]
        async fn root_path_redirect_preserves_query_without_trailing_slash() {
            let config = config(&["en", "es"], &[]);
            let route = simple_route("/", "root", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let response = request(&app, "/?ref=newsletter", &[]).await;
            assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/en?ref=newsletter")
            );
        }

        /// AC: every route is reachable under `/{locale}` for each supported
        /// locale, with zero hand-duplicated route definitions — one `Route`
        /// serves both `/en/posts` and `/es/posts`.
        #[tokio::test]
        async fn route_reachable_under_every_supported_locale() {
            let config = config(&["en", "es"], &[]);
            let route = simple_route("/posts", "posts", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let en = request(&app, "/en/posts", &[]).await;
            assert_eq!(en.status(), StatusCode::OK);
            assert_eq!(body_string(en).await, "en");

            let es = request(&app, "/es/posts", &[]).await;
            assert_eq!(es.status(), StatusCode::OK);
            assert_eq!(body_string(es).await, "es");
        }

        /// Codex review (P1): a duplicate entry in `supported_locales` must
        /// not panic at router-construction time (axum rejects nesting the
        /// same path twice as an overlapping route) — it should simply be
        /// deduped, and the locale should still route normally.
        #[tokio::test]
        async fn duplicate_supported_locale_does_not_panic_and_still_routes() {
            let config = config(&["en", "es", "en"], &[]);
            let route = simple_route("/posts", "posts", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let en = request(&app, "/en/posts", &[]).await;
            assert_eq!(en.status(), StatusCode::OK);
            assert_eq!(body_string(en).await, "en");

            let es = request(&app, "/es/posts", &[]).await;
            assert_eq!(es.status(), StatusCode::OK);
            assert_eq!(body_string(es).await, "es");
        }

        /// Codex review (P2): an empty-string entry in `supported_locales`
        /// must not panic at router-construction time — `.nest("/", ...)`
        /// (an empty locale segment) is a root nest, which axum rejects.
        /// Skip the malformed entry instead; the other, valid locale must
        /// still route normally.
        #[tokio::test]
        async fn malformed_locale_segment_does_not_panic_and_valid_locale_still_routes() {
            let config = config(&["en", "", "es"], &[]);
            let route = simple_route("/posts", "posts", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let en = request(&app, "/en/posts", &[]).await;
            assert_eq!(en.status(), StatusCode::OK);
            assert_eq!(body_string(en).await, "en");

            let es = request(&app, "/es/posts", &[]).await;
            assert_eq!(es.status(), StatusCode::OK);
            assert_eq!(body_string(es).await, "es");
        }

        /// Codex review (P1): an app that defines both `/foo` and the exact
        /// path its own locale-prefix nesting would generate for it
        /// (`/en/foo`, when `en` is supported) must not panic at
        /// router-construction time — the bare-path redirect (mounted at
        /// every locale-prefix-eligible path via `any()`, which claims every
        /// HTTP method) already owns `/en/foo`, so nesting `/foo`'s content
        /// under `/en` collides. Surfaced as a structured build error.
        #[tokio::test]
        async fn generated_locale_path_collision_is_a_build_error_not_a_panic() {
            let config = config(&["en", "es"], &[]);
            let foo = simple_route("/foo", "foo", false);
            let en_foo = simple_route("/en/foo", "en_foo", false);
            let err = try_build_router(vec![foo, en_foo], &config, test_state())
                .expect_err("a generated-path collision must be a build error, not a panic");
            match err {
                RouterBuildError::LocalePrefixPathCollision {
                    ref locale,
                    ref path,
                    ref generated,
                } => {
                    assert_eq!(locale, "en");
                    assert_eq!(path, "/foo");
                    assert_eq!(generated, "/en/foo");
                }
                other => panic!("expected LocalePrefixPathCollision, got {other:?}"),
            }
        }

        /// Codex review (P1): a generated locale path can collide with an
        /// existing route that has the SAME matchit shape but a DIFFERENT
        /// capture name (`/en/users/{id}` generated from `/users/{id}`, vs.
        /// an existing `/en/users/{slug}`) — axum's `Router::route` rejects
        /// this as a conflict regardless of the capture name, exactly like
        /// the exact-duplicate-path case, so it must be caught the same way.
        #[tokio::test]
        async fn generated_locale_path_collision_via_capture_name_mismatch_is_a_build_error() {
            let config = config(&["en"], &[]);
            let users_id = slug_route("/users/{id}", "users_id");
            let en_users_slug = slug_route("/en/users/{slug}", "en_users_slug");
            let err = try_build_router(vec![users_id, en_users_slug], &config, test_state())
                .expect_err("a capture-name-mismatched shape collision must be a build error");
            assert!(
                matches!(err, RouterBuildError::LocalePrefixPathCollision { .. }),
                "expected LocalePrefixPathCollision, got {err:?}"
            );
        }

        /// Codex review (P2): a legal cross-method exact-path match must
        /// NOT be flagged as a collision — axum merges the SAME path
        /// template across DIFFERENT methods. `GET /foo` generates
        /// `GET /en/foo`, which must coexist with a separately registered
        /// `POST /en/foo`.
        #[tokio::test]
        async fn exact_generated_path_with_different_method_is_not_a_collision() {
            let config = config(&["en"], &[]);
            let get_foo = simple_route("/foo", "foo_get", false);
            let post_en_foo = duplicate_test_route(http::Method::POST, "/en/foo", "post_en_foo");

            // Must build without panicking or erroring.
            let app = build_router(vec![get_foo, post_en_foo], &config, test_state());

            let get_resp = request(&app, "/en/foo", &[]).await;
            assert_eq!(
                get_resp.status(),
                StatusCode::OK,
                "GET /en/foo must resolve to the nested /foo content"
            );
        }

        /// Codex review (P1): a route mounted via `AppBuilder::scoped()`
        /// lives outside `route_list` entirely, but still mounts onto the
        /// same router (`mount_scoped_groups`, after this module returns) —
        /// a scoped route whose resolved path collides with a generated
        /// locale path must be caught here too, not just top-level routes.
        #[tokio::test]
        async fn generated_locale_path_colliding_with_a_scoped_route_is_a_build_error() {
            let config = config(&["en"], &[]);
            let foo = simple_route("/foo", "foo", false);
            let scoped_route = duplicate_test_route(http::Method::GET, "/foo", "scoped_foo");
            let group = crate::app::ScopedGroup {
                prefix: "/en".to_owned(),
                routes: vec![scoped_route],
                source: crate::route_listing::RouteSource::User,
                apply_layer: Box::new(|r| r),
            };
            let mut ctx = duplicate_test_ctx();
            ctx.scoped_groups.push(group);

            let err = super::try_build_router_inner(vec![foo], &config, test_state(), ctx)
                .expect_err(
                    "a scoped route colliding with a generated locale path must be a build error",
                );
            assert!(
                matches!(err, RouterBuildError::LocalePrefixPathCollision { .. }),
                "expected LocalePrefixPathCollision, got {err:?}"
            );
        }

        /// Codex review (P1): a generated locale path can collide with a
        /// framework-owned path — the health probes, mounted later via
        /// `mount_probe_endpoints`, are invisible to `collect_user_get_paths`
        /// (which only sees the raw, pre-locale-prefix `route_list`) and so
        /// weren't previously checked here either.
        #[tokio::test]
        async fn generated_locale_path_colliding_with_a_health_probe_is_a_build_error() {
            let mut config = config(&["en"], &[]);
            config.health.path = "/en/foo".to_owned();
            let foo = simple_route("/foo", "foo", false);

            let err = try_build_router(vec![foo], &config, test_state()).expect_err(
                "a health probe colliding with a generated locale path must be a build error",
            );
            assert!(
                matches!(err, RouterBuildError::LocalePrefixPathCollision { .. }),
                "expected LocalePrefixPathCollision, got {err:?}"
            );
        }

        /// Codex review (P2): a locale segment beginning with `:` (axum 0.7
        /// capture syntax) must not panic at router-construction time — axum
        /// 0.8's `Router::route` rejects it during assembly (the same
        /// restriction `InvalidMcpPath` guards for MCP mount paths).
        #[tokio::test]
        async fn colon_prefixed_locale_segment_does_not_panic() {
            let config = config(&["en", ":en"], &[]);
            let route = simple_route("/posts", "posts", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en"])));

            let en = request(&app, "/en/posts", &[]).await;
            assert_eq!(en.status(), StatusCode::OK);
            assert_eq!(body_string(en).await, "en");
        }

        /// Codex review (P2): a locale segment containing a query/fragment
        /// delimiter or whitespace (e.g. `"en?x"`, `"en#x"`, `"en y"`) must
        /// be skipped rather than nested — axum accepts it as a literal
        /// nest string, but a client parses `/en?x/foo` as path `/en` plus
        /// query `x/foo`, silently truncating the redirect target. Router
        /// construction must not panic, and the other, valid locale must
        /// still route normally.
        #[tokio::test]
        async fn locale_segment_with_uri_delimiter_does_not_panic_and_valid_locale_still_routes() {
            let config = config(&["en", "en?x", "en#x", "en y"], &[]);
            let route = simple_route("/posts", "posts", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en"])));

            let en = request(&app, "/en/posts", &[]).await;
            assert_eq!(en.status(), StatusCode::OK);
            assert_eq!(body_string(en).await, "en");
        }

        /// Codex review (P1): the bare-path redirect must claim only the
        /// methods actually registered at that path, not every method via
        /// `any()`. A user route at `POST /health` (a different method than
        /// the framework's own auto-mounted `GET /health` probe) must not
        /// have its redirect swallow GET too — that would collide with the
        /// probe's later auto-mount, which has no visibility into methods
        /// the redirect already claimed.
        #[tokio::test]
        async fn bare_path_redirect_claims_only_registered_methods_not_every_method() {
            async fn user_health_post() -> &'static str {
                "posted"
            }

            let config = config(&["en"], &[]);
            let route = Route {
                method: http::Method::POST,
                path: "/health",
                handler: axum::routing::post(user_health_post),
                name: "user_health_post",
                api_doc: crate::openapi::ApiDoc {
                    method: "POST",
                    path: "/health",
                    operation_id: "user_health_post",
                    success_status: 200,
                    ..Default::default()
                },
                repository: None,
                idempotency: crate::route::RouteIdempotency::Direct,
                timeout: crate::route::RouteTimeout::Inherit,
                seo: crate::seo::SeoRouteDefaults::EMPTY,
                api_version: None,
                sunset_opt_out: false,
            };

            // Before the fix, the redirect's `any()` claimed GET at /health
            // too, colliding with the framework's own auto-mounted probe and
            // panicking inside `axum::Router::route`.
            let app = build_router(vec![route], &config, test_state());

            let health = request(&app, "/health", &[]).await;
            assert_eq!(
                health.status(),
                StatusCode::OK,
                "the framework's own health probe must still respond at GET /health"
            );

            let redirected = request_method(&app, http::Method::POST, "/health", &[]).await;
            assert_eq!(redirected.status(), StatusCode::PERMANENT_REDIRECT);
            assert_eq!(
                redirected
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/en/health")
            );
        }

        /// Codex review (P2): `LocaleRoutingConfig` (and the default-locale
        /// negotiation fallback) must be built from the validated,
        /// deduplicated locale set — the same one actually nested — not the
        /// raw `supported_locales`. Otherwise a skipped invalid entry could
        /// still be selected as the negotiated locale (or as the fallback
        /// default) and 404, since no `/{locale}` nest exists for it.
        #[tokio::test]
        async fn skipped_invalid_locale_is_never_selected_by_negotiation() {
            let mut config = config(&["", "en"], &[]);
            config.i18n.default_locale = String::new();
            let route = simple_route("/posts", "posts", true);
            let app = build_router(vec![route], &config, test_state());

            // The empty "default" is invalid and skipped, so the bare-path
            // redirect must fall back to "en" — the only validly-nested
            // locale — not attempt an unreachable "//posts".
            let response = request(&app, "/posts", &[]).await;
            assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/en/posts"),
                "must redirect to the validly-nested locale, not the skipped empty default"
            );
        }

        /// AC: an unknown `{locale}` prefix 404s — no panic.
        #[tokio::test]
        async fn unknown_locale_prefix_is_404_not_panic() {
            let config = config(&["en", "es"], &[]);
            let route = simple_route("/posts", "posts", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let response = request(&app, "/zz/posts", &[]).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

        /// AC: a bare (non-prefixed) path 308-redirects to the negotiated
        /// locale's prefixed path, preserving the query string.
        #[tokio::test]
        async fn bare_path_redirects_preserving_query() {
            let config = config(&["en", "es"], &[]);
            let route = simple_route("/posts", "posts", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let response = request(&app, "/posts?sort=asc", &[]).await;
            assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/en/posts?sort=asc")
            );

            let response = request(
                &app,
                "/posts?sort=asc",
                &[(axum::http::header::ACCEPT_LANGUAGE.as_str(), "es")],
            )
            .await;
            assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/es/posts?sort=asc")
            );
        }

        /// AC: the URL prefix takes precedence over cookie/session/
        /// `Accept-Language`, with zero handler changes (the handler above
        /// takes a plain `Locale` parameter).
        #[tokio::test]
        async fn url_prefix_locale_wins_over_cookie_and_accept_language() {
            let config = config(&["en", "es"], &[]);
            let route = simple_route("/posts", "posts", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let response = request(
                &app,
                "/es/posts",
                &[
                    (axum::http::header::COOKIE.as_str(), "autumn_locale=en"),
                    (axum::http::header::ACCEPT_LANGUAGE.as_str(), "en"),
                ],
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(body_string(response).await, "es");
        }

        /// AC: configured prefixes are excluded from localization and from
        /// the bare-path redirect, so machine endpoints stay unprefixed.
        #[tokio::test]
        async fn excluded_prefix_stays_unprefixed_and_unredirected() {
            let config = config(&["en", "es"], &["/api"]);
            let route = simple_route("/api/status", "api_status", false);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let direct = request(&app, "/api/status", &[]).await;
            assert_eq!(
                direct.status(),
                StatusCode::OK,
                "excluded route must serve directly, not redirect"
            );

            let nested = request(&app, "/en/api/status", &[]).await;
            assert_eq!(
                nested.status(),
                StatusCode::NOT_FOUND,
                "excluded route must not be nested under a locale prefix"
            );
        }

        /// A trailing `/*` on a configured exclude prefix is equivalent to
        /// the bare prefix — `"/api"` and `"/api/*"` behave identically.
        #[tokio::test]
        async fn exclude_prefix_accepts_trailing_glob_suffix() {
            let config = config(&["en", "es"], &["/api/*"]);
            let route = simple_route("/api/status", "api_status", false);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let direct = request(&app, "/api/status", &[]).await;
            assert_eq!(direct.status(), StatusCode::OK);

            let nested = request(&app, "/en/api/status", &[]).await;
            assert_eq!(nested.status(), StatusCode::NOT_FOUND);
        }

        /// Codex review (P1): a bare `"/"` exclude entry (as
        /// `exclude_static_routes_from_locale_prefix` adds for a
        /// `#[static_get("/")]` route) must exclude exactly the root path,
        /// not be normalized away to an empty, always-non-matching prefix.
        #[tokio::test]
        async fn root_path_exclude_prefix_excludes_exactly_the_root() {
            let config = config(&["en", "es"], &["/"]);
            let root = simple_route("/", "root", false);
            let posts = simple_route("/posts", "posts", true);
            let app = build_router(vec![root, posts], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let bare_root = request(&app, "/", &[]).await;
            assert_eq!(
                bare_root.status(),
                StatusCode::OK,
                "excluded root route must serve directly, not redirect"
            );

            let nested_root = request(&app, "/en", &[]).await;
            assert_eq!(
                nested_root.status(),
                StatusCode::NOT_FOUND,
                "excluded root route must not be nested under a locale prefix"
            );

            // A "/" exclude entry must not swallow every other path — only
            // the exact root.
            let bare_posts = request(&app, "/posts", &[]).await;
            assert_eq!(
                bare_posts.status(),
                StatusCode::PERMANENT_REDIRECT,
                "\"/\" in the exclude list must not exclude unrelated routes"
            );
        }

        /// A path that merely starts with an exclude prefix's characters
        /// (`/apikeys` vs the configured `/api`) is NOT a prefix match — only
        /// an exact path or a `/`-delimited sub-path counts. Otherwise
        /// `/apikeys` would be wrongly swept into the excluded/unprefixed
        /// group alongside genuine `/api/*` routes.
        #[tokio::test]
        async fn exclude_prefix_does_not_match_unrelated_path_sharing_a_string_prefix() {
            let config = config(&["en", "es"], &["/api"]);
            let route = simple_route("/apikeys", "apikeys", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let bare = request(&app, "/apikeys", &[]).await;
            assert_eq!(
                bare.status(),
                StatusCode::PERMANENT_REDIRECT,
                "/apikeys must be treated as included (locale-prefixed), not excluded"
            );

            let nested = request(&app, "/en/apikeys", &[]).await;
            assert_eq!(
                nested.status(),
                StatusCode::OK,
                "/apikeys must be nested under the locale prefix like any other included route"
            );
        }

        /// A parameterized route (`/posts/{slug}`) is reachable through a
        /// locale nest exactly like a static path — dynamic-segment matching
        /// works the same whether or not the route is nested.
        #[tokio::test]
        async fn parameterized_route_reachable_under_locale_prefix() {
            let config = config(&["en", "es"], &[]);
            let route = slug_route("/posts/{slug}", "posts_show");
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let response = request(&app, "/es/posts/hello-world", &[]).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(body_string(response).await, "es:hello-world");
        }

        /// Codex review (P1): an exact-match exclusion (as populated by
        /// `exclude_static_routes_from_locale_prefix` for a `#[static_get]`
        /// route) must not behave like a *prefix* exclusion — excluding the
        /// literal static route `/posts` must not also swallow an unrelated
        /// dynamic sibling like `/posts/{slug}`, which shares only a string
        /// prefix, not a namespace.
        #[tokio::test]
        async fn exact_exclude_does_not_swallow_a_dynamic_sibling_route() {
            let mut config = config(&["en", "es"], &[]);
            config.i18n.locale_prefix_exclude_exact = vec!["/posts".to_owned()];
            let static_index = simple_route("/posts", "posts_index", false);
            let dynamic_child = slug_route("/posts/{slug}", "posts_show");
            let app = build_router(vec![static_index, dynamic_child], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let excluded = request(&app, "/posts", &[]).await;
            assert_eq!(
                excluded.status(),
                StatusCode::OK,
                "the exactly-excluded static route must stay unprefixed and unredirected"
            );

            let excluded_prefixed = request(&app, "/en/posts", &[]).await;
            assert_eq!(
                excluded_prefixed.status(),
                StatusCode::NOT_FOUND,
                "the exactly-excluded static route must not be nested under a locale prefix"
            );

            let sibling_prefixed = request(&app, "/en/posts/hello-world", &[]).await;
            assert_eq!(
                sibling_prefixed.status(),
                StatusCode::OK,
                "a dynamic sibling sharing the excluded path as a string prefix must \
                 still be locale-prefixed normally — an exact exclusion must not act \
                 like a prefix exclusion"
            );
            assert_eq!(body_string(sibling_prefixed).await, "en:hello-world");

            let sibling_bare = request(&app, "/posts/hello-world", &[]).await;
            assert_eq!(
                sibling_bare.status(),
                StatusCode::PERMANENT_REDIRECT,
                "the dynamic sibling's bare path must still redirect, since only the \
                 exact literal `/posts` is excluded"
            );
        }

        /// Core assumption underpinning `locale_switcher`/hreflang
        /// (documented on `widgets::localized_path` and
        /// `seo::locale_alternates`): axum's `nest()` strips the matched
        /// locale segment before extraction, so a handler's plain `Uri`
        /// extractor inside the nest sees the locale-stripped path — not the
        /// `/es/posts` the client actually requested.
        #[tokio::test]
        async fn uri_extractor_inside_locale_nest_sees_locale_stripped_path() {
            let config = config(&["en", "es"], &[]);
            let route = uri_probe_route("/posts", "posts_uri_probe");
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let response = request(&app, "/es/posts", &[]).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                body_string(response).await,
                "/posts",
                "Uri extractor inside a locale nest must see the prefix-stripped path"
            );
        }

        /// The redirect handler is mounted via `any()`, so it must redirect
        /// non-GET methods too, not just the GET case every other test uses.
        #[tokio::test]
        async fn bare_path_redirect_applies_to_non_get_methods() {
            let config = config(&["en", "es"], &[]);
            let route = make_post_route("/posts", "posts_create");
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let response = request_method(&app, http::Method::POST, "/posts", &[]).await;
            assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/en/posts")
            );
        }

        /// Two routes sharing one path under different HTTP methods must
        /// still dedup to a single bare-path redirect registration — the
        /// `included_paths.sort_unstable(); included_paths.dedup();` step in
        /// `mount_user_routes` exists precisely to prevent `Router::route`
        /// panicking on the same path registered twice.
        #[tokio::test]
        async fn same_path_different_methods_dedups_to_one_redirect_route() {
            let config = config(&["en", "es"], &[]);
            let get_route = simple_route("/posts", "posts_list", true);
            let post_route = make_post_route("/posts", "posts_create");
            let app = build_router(vec![get_route, post_route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es"])));

            let get_redirect = request(&app, "/posts", &[]).await;
            assert_eq!(get_redirect.status(), StatusCode::PERMANENT_REDIRECT);
            let post_redirect = request_method(&app, http::Method::POST, "/posts", &[]).await;
            assert_eq!(post_redirect.status(), StatusCode::PERMANENT_REDIRECT);

            let en_get = request(&app, "/en/posts", &[]).await;
            assert_eq!(en_get.status(), StatusCode::OK);
            let en_post = request_method(&app, http::Method::POST, "/en/posts", &[]).await;
            assert_eq!(en_post.status(), StatusCode::OK);
        }

        /// A region-subtagged locale code (`es-MX`) works as a literal nest
        /// prefix like any other configured locale, and an uppercase variant
        /// of a configured code is treated as unknown (404) — the nest
        /// prefix is a literal path segment, not case-negotiated.
        #[tokio::test]
        async fn region_subtag_locale_nests_and_prefix_is_case_sensitive() {
            let config = config(&["en", "es-MX"], &[]);
            let route = simple_route("/posts", "posts", true);
            let app = build_router(vec![route], &config, test_state())
                .layer(axum::Extension(bundle(&["en", "es-MX"])));

            let response = request(&app, "/es-MX/posts", &[]).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(body_string(response).await, "es-MX");

            let wrong_case = request(&app, "/ES-MX/posts", &[]).await;
            assert_eq!(
                wrong_case.status(),
                StatusCode::NOT_FOUND,
                "the locale nest prefix is a literal path segment, not case-negotiated"
            );
        }

        /// `supported_locales = []` is a degenerate config (locale-prefix
        /// routing on, nothing to prefix with): no nest is created, and the
        /// bare-path redirect is skipped rather than 308-ing to an
        /// unreachable `/{default_locale}/...` target — a clean 404 beats a
        /// redirect-then-404 round trip.
        #[tokio::test]
        async fn empty_supported_locales_serves_clean_404_not_redirect_to_nowhere() {
            let mut config = AutumnConfig::default();
            config.i18n.locale_prefix_enabled = true;
            config.i18n.supported_locales = vec![];

            let route = simple_route("/posts", "posts", false);
            let app = build_router(vec![route], &config, test_state());

            let response = request(&app, "/posts", &[]).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

        /// Codex review (P2): a misconfigured `default_locale` absent from
        /// `supported_locales` (e.g. `default_locale = "fr"`,
        /// `supported_locales = ["en"]`) must not negotiate a bare request to
        /// an unreachable `/fr/...` target — only `en` is actually nested.
        /// The negotiation fallback clamps to the first supported (i.e.
        /// actually mounted) locale instead.
        #[tokio::test]
        async fn misconfigured_default_locale_falls_back_to_a_mounted_locale() {
            let mut config = config(&["en"], &[]);
            config.i18n.default_locale = "fr".to_owned();

            let route = simple_route("/posts", "posts", true);
            // No bundle — exercises the router's own `LocaleRoutingConfig`
            // fallback, same as `locale_prefix_redirect_works_without_an_i18n_bundle`.
            let app = build_router(vec![route], &config, test_state());

            let redirected = request(&app, "/posts", &[]).await;
            assert_eq!(redirected.status(), StatusCode::PERMANENT_REDIRECT);
            assert_eq!(
                redirected
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/en/posts"),
                "must redirect to a locale that's actually mounted, not the \
                 misconfigured default_locale"
            );

            let target = request(&app, "/en/posts", &[]).await;
            assert_eq!(target.status(), StatusCode::OK);
        }

        /// Routes registered via `AppBuilder::scoped()` are mounted by a
        /// separate pipeline (`mount_scoped_groups`, which runs after
        /// locale-prefix mounting) and are therefore untouched by
        /// locale-prefix routing — no locale nest, no bare-path redirect —
        /// exactly as if the flag were off. This guards the framework's
        /// documented boundary: only the top-level `routes![...]` table is
        /// locale-prefixed.
        #[tokio::test]
        async fn scoped_group_routes_are_not_locale_prefixed() {
            let config = config(&["en", "es"], &[]);
            let scoped_route = duplicate_test_route(http::Method::GET, "/status", "scoped_status");
            let group = crate::app::ScopedGroup {
                prefix: "/admin".to_owned(),
                routes: vec![scoped_route],
                source: crate::route_listing::RouteSource::User,
                apply_layer: Box::new(|r| r),
            };
            let mut ctx = duplicate_test_ctx();
            ctx.scoped_groups.push(group);

            let router = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
                .expect("router with a scoped group under locale-prefix routing should build")
                .layer(axum::Extension(bundle(&["en", "es"])));

            let direct = request(&router, "/admin/status", &[]).await;
            assert_eq!(
                direct.status(),
                StatusCode::OK,
                "scoped route must serve directly, unprefixed"
            );

            let nested = request(&router, "/en/admin/status", &[]).await;
            assert_eq!(
                nested.status(),
                StatusCode::NOT_FOUND,
                "scoped routes are mounted after locale-prefix nesting, so no nest exists for them"
            );
        }
    }

    /// Loads a layered `autumn.toml` for `profile` via `MockEnv` (no process
    /// env, no `set_current_dir`) and reports the status `/_stories` returns.
    #[cfg(feature = "maud")]
    async fn stories_status_for_layered_profile(toml: &str, profile: &str) -> StatusCode {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), toml).expect("write autumn.toml");
        let env = crate::config::MockEnv::new()
            .with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap())
            .with("AUTUMN_ENV", profile);
        let mut config = AutumnConfig::load_with_env(&env).expect("layered config should load");
        config.security.trusted_hosts.hosts = vec!["example.com".to_owned()];

        let router = build_router(Vec::new(), &config, stories_state_with_builtin());
        get_with_host(router, "/_stories").await.status()
    }

    /// T5 (AC6): profile-scoped gating works both ways through the existing
    /// config layering — routes mount iff the resolved flag is true.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn story_routes_mount_iff_resolved_profile_flag() {
        // Private app: dev-only gallery, absent in prod.
        let dev_only = r"
[stories]
enabled = false

[profile.dev.stories]
enabled = true
";
        assert_eq!(
            stories_status_for_layered_profile(dev_only, "dev").await,
            StatusCode::OK,
            "dev profile override must mount the gallery"
        );
        assert_eq!(
            stories_status_for_layered_profile(dev_only, "prod").await,
            StatusCode::NOT_FOUND,
            "prod must not mount the gallery when only dev enables it"
        );

        // Public showcase: enabled in prod, absent in dev.
        let public_showcase = r"
[stories]
enabled = false

[profile.prod.stories]
enabled = true
";
        assert_eq!(
            stories_status_for_layered_profile(public_showcase, "prod").await,
            StatusCode::OK,
            "prod profile override must mount the gallery for a public showcase"
        );
        assert_eq!(
            stories_status_for_layered_profile(public_showcase, "dev").await,
            StatusCode::NOT_FOUND,
            "dev must not mount the gallery when only prod enables it"
        );
    }

    /// T6 (AC7): a custom app story registered via
    /// `StoryGallery::builtin().extend(...)` is served alongside builtins.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn custom_story_served_alongside_builtins() {
        let custom = crate::stories::story! {
            "App",
            "Greeting",
            {
                maud::html! { span class="app-greeting" { "hi from the app" } }
            }
        };
        let state = test_state();
        state.insert_extension(
            crate::stories::StoryGallery::builtin()
                .extend([custom])
                .into_registry(),
        );

        let router = build_router(Vec::new(), &story_gallery_config(), state);

        let detail = get_with_host(router.clone(), "/_stories/greeting").await;
        assert_eq!(detail.status(), StatusCode::OK);
        let body = response_text(detail).await;
        assert!(
            body.contains("hi from the app"),
            "custom story must render at its slug: {body}"
        );

        let index = get_with_host(router, "/_stories").await;
        let body = response_text(index).await;
        assert!(
            body.contains("Greeting"),
            "index must list the custom story: {body}"
        );
        assert!(
            body.contains("App"),
            "index must show the custom story's group: {body}"
        );
        assert!(
            body.contains("Data table"),
            "builtins must still be listed alongside the custom story: {body}"
        );
    }

    /// Review follow-up (#1526): with `security.headers.csp_nonce.enabled =
    /// true` the default CSP's `style-src` drops `'unsafe-inline'` in favor of
    /// a per-request nonce, so the gallery's inline `<style>` must carry the
    /// exact nonce the CSP header advertises or browsers block all of the
    /// gallery chrome CSS.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn story_pages_inline_style_carries_csp_header_nonce() {
        let mut config = story_gallery_config();
        config.security.headers.csp_nonce.enabled = true;

        let router = build_router(Vec::new(), &config, stories_state_with_builtin());

        for uri in ["/_stories", "/_stories/data-table"] {
            let response = get_with_host(router.clone(), uri).await;
            assert_eq!(response.status(), StatusCode::OK);

            let csp = response
                .headers()
                .get("content-security-policy")
                .expect("CSP header must be present")
                .to_str()
                .unwrap()
                .to_owned();
            let nonce = csp
                .split("'nonce-")
                .nth(1)
                .and_then(|rest| rest.split('\'').next())
                .unwrap_or_else(|| panic!("CSP header must advertise a nonce: {csp}"))
                .to_owned();
            assert!(!nonce.is_empty(), "advertised nonce must be non-empty");

            let body = response_text(response).await;
            assert!(
                body.contains(&format!(r#"<style nonce="{nonce}">"#)),
                "{uri} inline style must carry the CSP header nonce {nonce}: {body}"
            );
        }
    }

    /// T17 (AC5, R12): enabled config but no registry extension (the user
    /// forgot `with_story_gallery`) serves a friendly empty state, not a 500.
    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn enabled_without_registry_shows_empty_state() {
        let router = build_router(Vec::new(), &story_gallery_config(), test_state());

        let response = get_with_host(router, "/_stories").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_text(response).await;
        assert!(
            body.contains("with_story_gallery"),
            "empty state should point at AppBuilder::with_story_gallery: {body}"
        );
    }

    #[tokio::test]
    async fn apply_csrf_middleware_blocks_without_token_when_enabled() {
        let mut config = AutumnConfig::default();
        config.security.csrf.enabled = true;

        let base: axum::Router<AppState> =
            axum::Router::new().route("/form", axum::routing::post(|| async { "posted" }));
        let router = apply_csrf_middleware(base, &config, None).with_state(test_state());

        // POST without CSRF token should be rejected
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/form")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_ne!(
            response.status(),
            StatusCode::OK,
            "POST without CSRF token should be rejected when CSRF is enabled"
        );
    }

    #[test]
    fn join_nested_path_normalizes_like_axum() {
        // Reviewer's reported case: scope "/api" + child "/" must
        // produce "/api", not "/api/" — otherwise a user-configured
        // openapi_json_path("/api") won't match the effective mount
        // point and the collision check is unreliable.
        assert_eq!(super::join_nested_path("/api", "/"), "/api");
        // Trailing slash on the prefix is preserved for the root child:
        // axum mounts `nest("/api/", route("/"))` at "/api/" and reports
        // `MatchedPath` as "/api/" (verified by
        // `join_nested_path_matches_axum_matched_path`), so the joined key
        // must keep the slash or the runtime lookup misses.
        assert_eq!(super::join_nested_path("/api/", "/"), "/api/");
        // Normal case: prefix + child.
        assert_eq!(super::join_nested_path("/api", "/users"), "/api/users");
        // Trailing slash on prefix + child starting with slash doesn't
        // produce doubled slashes.
        assert_eq!(super::join_nested_path("/api/", "/users"), "/api/users");
        // Root prefix handles sensibly.
        assert_eq!(super::join_nested_path("", "/"), "/");
        assert_eq!(super::join_nested_path("", "/users"), "/users");
    }

    /// Pins `join_nested_path` to axum's real `MatchedPath` so the per-route
    /// timeout table (and the `OpenAPI` collision check) key by exactly the
    /// string the runtime looks up. The trailing-slash root child is the
    /// subtle case: `nest("/api/", route("/"))` is served at "/api/", not
    /// "/api".
    #[tokio::test]
    async fn join_nested_path_matches_axum_matched_path() {
        use axum::routing::get;
        async fn matched(mp: Option<axum::extract::MatchedPath>) -> String {
            mp.map(|m| m.as_str().to_owned()).unwrap_or_default()
        }
        // (nest prefix, child route, request path that reaches the child)
        for (prefix, child, req) in [
            ("/api", "/", "/api"),
            ("/api/", "/", "/api/"),
            ("/api", "/users", "/api/users"),
            ("/api/", "/users", "/api/users"),
        ] {
            let sub = axum::Router::new().route(child, get(matched));
            let app: axum::Router = axum::Router::new().nest(prefix, sub);
            let resp = tower::ServiceExt::oneshot(
                app,
                axum::http::Request::builder()
                    .uri(req)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(resp.status(), http::StatusCode::OK, "{prefix} + {child}");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let axum_matched = String::from_utf8(body.to_vec()).unwrap();
            assert_eq!(
                super::join_nested_path(prefix, child),
                axum_matched,
                "join_nested_path must equal axum MatchedPath for nest({prefix:?}, {child:?})"
            );
        }
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn try_build_router_detects_scoped_root_collision() {
        // Scope "/api" + child "/" mounts axum's handler at "/api"
        // (not "/api/"). The collision check must use the same
        // normalization or we'd miss this overlap.
        use crate::openapi::{ApiDoc, OpenApiConfig};
        async fn child() -> &'static str {
            "inner"
        }
        let group = crate::app::ScopedGroup {
            prefix: "/api".to_owned(),
            routes: vec![Route {
                method: http::Method::GET,
                path: "/",
                handler: axum::routing::get(child),
                name: "root",
                api_doc: ApiDoc {
                    method: "GET",
                    path: "/",
                    operation_id: "root",
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
            source: crate::route_listing::RouteSource::User,
            apply_layer: Box::new(|r| r),
        };

        let openapi = OpenApiConfig::new("Demo", "1.0.0").openapi_json_path("/api");
        let config = AutumnConfig::default();
        let ctx = RouterContext {
            exception_filters: Vec::new(),
            scoped_groups: vec![group],
            merge_routers: Vec::new(),
            nest_routers: Vec::new(),
            declared_routes: Vec::new(),
            custom_layers: Vec::new(),
            static_gate_layers: Vec::new(),
            #[cfg(feature = "maud")]
            error_page_renderer: None,
            session_store: None,
            openapi: Some(openapi),
            #[cfg(feature = "mcp")]
            mcp: None,
        };
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("scope '/api' + child '/' should collide with openapi path '/api'");
        assert!(matches!(
            err,
            RouterBuildError::OpenApiPathCollision {
                field: "openapi_json_path",
                ..
            }
        ));
    }

    /// The widget stylesheet route merges a GET unconditionally whenever
    /// `maud` is on, before the late-merged `OpenAPI` router — an
    /// `openapi_json_path` configured to the same path must be rejected by
    /// the preflight, not panic in `router.merge`.
    #[cfg(all(feature = "openapi", feature = "maud"))]
    #[test]
    fn try_build_router_detects_widgets_css_path_collision() {
        use crate::openapi::OpenApiConfig;

        let openapi =
            OpenApiConfig::new("Demo", "1.0.0").openapi_json_path(crate::ui::WIDGETS_CSS_PATH);
        let config = AutumnConfig::default();
        let ctx = RouterContext {
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
            openapi: Some(openapi),
            #[cfg(feature = "mcp")]
            mcp: None,
        };
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx).expect_err(
            "openapi_json_path colliding with the widget stylesheet route should be rejected",
        );
        assert!(matches!(
            err,
            RouterBuildError::OpenApiPathCollision {
                field: "openapi_json_path",
                ..
            }
        ));
    }

    /// Same as above for the flash stylesheet route (pre-existing gap, same
    /// class of bug: the flash CSS route was also missing from
    /// `collect_claimed_get_paths`).
    #[cfg(all(feature = "openapi", feature = "flash"))]
    #[test]
    fn try_build_router_detects_flash_css_path_collision() {
        use crate::openapi::OpenApiConfig;

        let openapi =
            OpenApiConfig::new("Demo", "1.0.0").openapi_json_path(crate::flash::FLASH_CSS_PATH);
        let config = AutumnConfig::default();
        let ctx = RouterContext {
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
            openapi: Some(openapi),
            #[cfg(feature = "mcp")]
            mcp: None,
        };
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx).expect_err(
            "openapi_json_path colliding with the flash stylesheet route should be rejected",
        );
        assert!(matches!(
            err,
            RouterBuildError::OpenApiPathCollision {
                field: "openapi_json_path",
                ..
            }
        ));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn extract_path_params_matches_macro_behavior() {
        // Normal multi-param routes.
        assert_eq!(
            super::extract_path_params("/orgs/{org_id}/users/{id}"),
            vec!["org_id".to_owned(), "id".to_owned()]
        );
        assert_eq!(
            super::extract_path_params("/users/{id}/posts/{slug}"),
            vec!["id".to_owned(), "slug".to_owned()]
        );
        assert!(super::extract_path_params("/static").is_empty());

        // `:constraint` suffixes are stripped to the bare name.
        assert_eq!(
            super::extract_path_params("/users/{id:[0-9]+}"),
            vec!["id".to_owned()]
        );
        // Regex constraint containing its own braces still yields just `id`.
        assert_eq!(
            super::extract_path_params("{id:[0-9]{1,3}}"),
            vec!["id".to_owned()]
        );

        // Escaped literal braces (`{{` / `}}`) are matchit literals, NOT
        // params, and must emit nothing (mirrors the macro's escape skip).
        assert!(super::extract_path_params("{{hello}}").is_empty());
        assert_eq!(
            super::extract_path_params("{{literal}}/{id}"),
            vec!["id".to_owned()]
        );

        // #1721 unbalanced/malformed cases: no phantom or brace-carrying params.
        assert!(super::extract_path_params("{{}").is_empty());
        assert!(super::extract_path_params("{a{b}").is_empty());
        assert!(super::extract_path_params("{").is_empty());
        assert!(super::extract_path_params("}").is_empty());
        assert!(super::extract_path_params("{}").is_empty());
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn extract_path_params_handles_unbalanced_braces() {
        // Regression for #1721: unbalanced/malformed braces must never yield a
        // param name that still contains a brace character. The brace-free
        // guard drops any candidate whose inner segment retains a stray brace,
        // so the emitted names are always non-empty and brace-free (and stray
        // braces yield no spurious params).
        for path in ["{{}", "{", "}", "{a{b}"] {
            for name in super::extract_path_params(path) {
                assert!(
                    !name.contains('{') && !name.contains('}'),
                    "param name should be brace-free for {path:?}: {name:?}"
                );
                assert!(
                    !name.is_empty(),
                    "param name should be non-empty for {path:?}"
                );
            }
        }
        // `"{{}"` yields no param: the leading `{{` is an escaped literal brace
        // that is skipped, leaving only a stray `}`.
        assert!(super::extract_path_params("{{}").is_empty());
        // `"{a{b}"` yields no param: the inner segment `"a{b"` still holds a
        // brace, so the brace-free guard drops it.
        assert!(super::extract_path_params("{a{b}").is_empty());
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn openapi_merges_scoped_prefix_path_params() {
        use crate::openapi::{ApiDoc, OpenApiConfig};

        // Scope prefix has `{org_id}`; the child route has `{id}`. The
        // generated ApiDoc must declare BOTH parameters, or Swagger
        // validators reject the document for referencing undeclared
        // path params.
        async fn handler() -> &'static str {
            "ok"
        }
        let child = Route {
            method: http::Method::GET,
            path: "/users/{id}",
            handler: axum::routing::get(handler),
            name: "child",
            api_doc: ApiDoc {
                method: "GET",
                path: "/users/{id}",
                operation_id: "child",
                path_params: &["id"],
                success_status: 200,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        };
        let group = crate::app::ScopedGroup {
            prefix: "/orgs/{org_id}".to_owned(),
            routes: vec![child],
            source: crate::route_listing::RouteSource::User,
            apply_layer: Box::new(|r| r),
        };

        let config = OpenApiConfig::new("Demo", "1.0.0");
        let router = super::build_openapi_router(&[], &[group], Some(&config), "autumn.sid", &[])
            .expect("openapi sub-router builds")
            .expect("openapi sub-router present when config is Some");
        let state = test_state();
        let router = router.with_state(state);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let spec: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let params = &spec["paths"]["/orgs/{org_id}/users/{id}"]["get"]["parameters"];
        let names: Vec<&str> = params
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"org_id"), "missing org_id: {names:?}");
        assert!(names.contains(&"id"), "missing id: {names:?}");
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn openapi_documents_configured_session_cookie_name() {
        use crate::openapi::{ApiDoc, OpenApiConfig};

        async fn handler() -> &'static str {
            "ok"
        }

        let route = Route {
            method: http::Method::GET,
            path: "/protected",
            handler: axum::routing::get(handler),
            name: "protected",
            api_doc: ApiDoc {
                method: "GET",
                path: "/protected",
                operation_id: "protected",
                success_status: 200,
                secured: true,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        };

        let protected_routes = vec![route];
        let config = OpenApiConfig::new("Demo", "1.0.0");
        let docs_router =
            super::build_openapi_router(&protected_routes, &[], Some(&config), "demo.sid", &[])
                .expect("openapi sub-router builds")
                .expect("openapi sub-router present when config is Some");
        let docs_router = docs_router.with_state(test_state());

        let response = docs_router
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let spec: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let schemes = &spec["components"]["securitySchemes"];

        assert_eq!(schemes["SessionAuth"]["type"], "apiKey");
        assert_eq!(schemes["SessionAuth"]["in"], "cookie");
        assert_eq!(schemes["SessionAuth"]["name"], "demo.sid");
        assert!(
            schemes.get("BearerAuth").is_none(),
            "secured routes must not be documented as bearer JWT routes"
        );
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn openapi_rejects_json_path_without_leading_slash() {
        let config =
            crate::openapi::OpenApiConfig::new("Demo", "1.0.0").openapi_json_path("openapi.json");
        let err = super::build_openapi_router(&[], &[], Some(&config), "autumn.sid", &[])
            .expect_err("non-slash path should be rejected");
        assert!(matches!(
            err,
            RouterBuildError::InvalidOpenApiPath {
                field: "openapi_json_path",
                ..
            }
        ));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn openapi_rejects_path_with_captures() {
        // `{id}` captures would be a typo for a mount path — the
        // endpoints are static. Catch it before axum panics.
        let config =
            crate::openapi::OpenApiConfig::new("Demo", "1.0.0").openapi_json_path("/docs/{id}");
        let err = super::build_openapi_router(&[], &[], Some(&config), "autumn.sid", &[])
            .expect_err("captures should be rejected");
        assert!(matches!(err, RouterBuildError::InvalidOpenApiPath { .. }));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn openapi_rejects_path_with_unbalanced_brace() {
        let config =
            crate::openapi::OpenApiConfig::new("Demo", "1.0.0").openapi_json_path("/docs/{id");
        let err = super::build_openapi_router(&[], &[], Some(&config), "autumn.sid", &[])
            .expect_err("unbalanced brace should be rejected");
        assert!(matches!(err, RouterBuildError::InvalidOpenApiPath { .. }));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn openapi_rejects_path_with_wildcard() {
        let config =
            crate::openapi::OpenApiConfig::new("Demo", "1.0.0").openapi_json_path("/docs/*rest");
        let err = super::build_openapi_router(&[], &[], Some(&config), "autumn.sid", &[])
            .expect_err("wildcard should be rejected");
        assert!(matches!(err, RouterBuildError::InvalidOpenApiPath { .. }));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn openapi_rejects_path_with_double_slash() {
        let config =
            crate::openapi::OpenApiConfig::new("Demo", "1.0.0").openapi_json_path("//docs");
        let err = super::build_openapi_router(&[], &[], Some(&config), "autumn.sid", &[])
            .expect_err("double-slash should be rejected");
        assert!(matches!(err, RouterBuildError::InvalidOpenApiPath { .. }));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn openapi_rejects_swagger_ui_path_without_leading_slash() {
        let config = crate::openapi::OpenApiConfig::new("Demo", "1.0.0")
            .swagger_ui_path(Some("docs".to_owned()));
        let err = super::build_openapi_router(&[], &[], Some(&config), "autumn.sid", &[])
            .expect_err("non-slash path should be rejected");
        assert!(matches!(
            err,
            RouterBuildError::InvalidOpenApiPath {
                field: "swagger_ui_path",
                ..
            }
        ));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn openapi_rejects_empty_json_path() {
        let config = crate::openapi::OpenApiConfig::new("Demo", "1.0.0").openapi_json_path("");
        let err = super::build_openapi_router(&[], &[], Some(&config), "autumn.sid", &[])
            .expect_err("empty path should be rejected");
        assert!(matches!(err, RouterBuildError::InvalidOpenApiPath { .. }));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn openapi_accepts_valid_paths() {
        let config = crate::openapi::OpenApiConfig::new("Demo", "1.0.0")
            .openapi_json_path("/api-docs")
            .swagger_ui_path(Some("/ui".to_owned()));
        let out = super::build_openapi_router(&[], &[], Some(&config), "autumn.sid", &[])
            .expect("valid paths must not error");
        assert!(out.is_some());
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn openapi_rejects_duplicate_json_and_swagger_paths() {
        let config = crate::openapi::OpenApiConfig::new("Demo", "1.0.0")
            .openapi_json_path("/docs")
            .swagger_ui_path(Some("/docs".to_owned()));
        let err = super::build_openapi_router(&[], &[], Some(&config), "autumn.sid", &[])
            .expect_err("colliding paths should be rejected before axum panics");
        assert!(matches!(
            err,
            RouterBuildError::DuplicateOpenApiPath { ref path } if path == "/docs"
        ));
    }

    #[cfg(feature = "openapi")]
    async fn collision_test_handler() -> &'static str {
        "user"
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn try_build_router_rejects_openapi_path_colliding_with_user_route() {
        let mut config = AutumnConfig::default();
        config.actuator.prefix = "/ops".to_owned();
        let openapi =
            crate::openapi::OpenApiConfig::new("Demo", "1.0.0").openapi_json_path("/my-api-docs");

        let user_route = Route {
            method: http::Method::GET,
            path: "/my-api-docs",
            handler: axum::routing::get(collision_test_handler),
            name: "collides",
            api_doc: crate::openapi::ApiDoc {
                method: "GET",
                path: "/my-api-docs",
                operation_id: "collides",
                success_status: 200,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        };

        let ctx = RouterContext {
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
            openapi: Some(openapi),
            #[cfg(feature = "mcp")]
            mcp: None,
        };
        let err = super::try_build_router_inner(vec![user_route], &config, test_state(), ctx)
            .expect_err("user-owned path should prevent OpenAPI mount");
        assert!(matches!(
            err,
            RouterBuildError::OpenApiPathCollision { field: "openapi_json_path", ref path } if path == "/my-api-docs"
        ));
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn try_build_router_rejects_openapi_path_colliding_with_framework_route() {
        let config = AutumnConfig::default(); // /actuator/health is a GET by default
        let openapi = crate::openapi::OpenApiConfig::new("Demo", "1.0.0")
            .openapi_json_path("/actuator/health");
        let ctx = RouterContext {
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
            openapi: Some(openapi),
            #[cfg(feature = "mcp")]
            mcp: None,
        };
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("framework-owned path should prevent OpenAPI mount");
        assert!(matches!(
            err,
            RouterBuildError::OpenApiPathCollision {
                field: "openapi_json_path",
                ..
            }
        ));
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn try_build_router_rejects_swagger_ui_asset_path_colliding_with_user_route() {
        let config = AutumnConfig::default();
        let openapi = crate::openapi::OpenApiConfig::new("Demo", "1.0.0");

        let user_route = Route {
            method: http::Method::GET,
            path: "/swagger-ui/swagger-ui.css",
            handler: axum::routing::get(collision_test_handler),
            name: "swagger-ui-asset-collides",
            api_doc: crate::openapi::ApiDoc {
                method: "GET",
                path: "/swagger-ui/swagger-ui.css",
                operation_id: "swagger_ui_asset_collides",
                success_status: 200,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        };

        let ctx = RouterContext {
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
            openapi: Some(openapi),
            #[cfg(feature = "mcp")]
            mcp: None,
        };
        let err = super::try_build_router_inner(vec![user_route], &config, test_state(), ctx)
            .expect_err("swagger ui asset path should be reserved");
        assert!(matches!(
            err,
            RouterBuildError::OpenApiPathCollision {
                field: "swagger_ui_path",
                ref path,
            } if path == "/swagger-ui/swagger-ui.css"
        ));
    }

    #[cfg(all(feature = "openapi", feature = "htmx"))]
    #[tokio::test]
    async fn try_build_router_rejects_openapi_path_colliding_with_htmx_csrf_route() {
        let config = AutumnConfig::default();
        let openapi = crate::openapi::OpenApiConfig::new("Demo", "1.0.0")
            .openapi_json_path(crate::htmx::HTMX_CSRF_JS_PATH);
        let ctx = RouterContext {
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
            openapi: Some(openapi),
            #[cfg(feature = "mcp")]
            mcp: None,
        };
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("htmx csrf helper path should be reserved");
        assert!(matches!(
            err,
            RouterBuildError::OpenApiPathCollision {
                field: "openapi_json_path",
                ref path,
            } if path == crate::htmx::HTMX_CSRF_JS_PATH
        ));
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn try_build_router_rejects_openapi_path_under_nest_prefix() {
        // Nesting `/api` means that router owns everything under
        // `/api/...`. Mounting OpenAPI at `/api/docs` would either
        // panic on merge or silently lose one of the routes, so the
        // collision check rejects it.
        let config = AutumnConfig::default();
        let openapi =
            crate::openapi::OpenApiConfig::new("Demo", "1.0.0").openapi_json_path("/api/docs");
        let nested = axum::Router::<AppState>::new()
            .route("/inner", axum::routing::get(|| async { "inner" }));
        let ctx = RouterContext {
            exception_filters: Vec::new(),
            scoped_groups: Vec::new(),
            merge_routers: Vec::new(),
            nest_routers: vec![("/api".to_owned(), nested)],
            declared_routes: Vec::new(),
            custom_layers: Vec::new(),
            static_gate_layers: Vec::new(),
            #[cfg(feature = "maud")]
            error_page_renderer: None,
            session_store: None,
            openapi: Some(openapi),
            #[cfg(feature = "mcp")]
            mcp: None,
        };
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("OpenAPI path under a nest prefix should collide");
        assert!(matches!(
            err,
            RouterBuildError::OpenApiPathCollision {
                field: "openapi_json_path",
                ref path,
            } if path == "/api/docs"
        ));
    }

    #[cfg(all(feature = "openapi", feature = "mail"))]
    #[tokio::test]
    async fn try_build_router_rejects_openapi_path_on_unsubscribe_endpoint() {
        // The default one-click unsubscribe endpoint merges a GET at
        // `/_autumn/unsubscribe` before the late-merged OpenAPI router, so the
        // collision preflight must reserve it — otherwise mounting OpenAPI there
        // panics in `router.merge` instead of surfacing the typed collision.
        let mut config = AutumnConfig::default();
        config.mail.mount_unsubscribe_endpoint = true;
        config.mail.unsubscribe_base_url = Some("https://app.example.com".to_owned());
        assert!(config.mail.should_mount_unsubscribe_endpoint());
        let openapi = crate::openapi::OpenApiConfig::new("Demo", "1.0.0")
            .openapi_json_path(crate::mail::UNSUBSCRIBE_PATH);
        let ctx = RouterContext {
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
            openapi: Some(openapi),
            #[cfg(feature = "mcp")]
            mcp: None,
        };
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("unsubscribe endpoint path should be reserved");
        assert!(matches!(
            err,
            RouterBuildError::OpenApiPathCollision {
                field: "openapi_json_path",
                ref path,
            } if path == crate::mail::UNSUBSCRIBE_PATH
        ));
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn try_build_router_rejects_openapi_path_on_job_status_endpoint() {
        // The tracked-job status endpoint merges a GET at
        // `/_autumn/jobs/{token}` before the late-merged OpenAPI router (on by
        // default), so the collision preflight must reserve it too.
        let config = AutumnConfig::default();
        assert!(config.jobs.tracking.route_enabled);
        let openapi = crate::openapi::OpenApiConfig::new("Demo", "1.0.0")
            .openapi_json_path(crate::job_tracking::JOB_STATUS_ROUTE_PATH);
        let ctx = RouterContext {
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
            openapi: Some(openapi),
            #[cfg(feature = "mcp")]
            mcp: None,
        };
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("job status endpoint path should be reserved");
        assert!(matches!(
            err,
            RouterBuildError::OpenApiPathCollision {
                field: "openapi_json_path",
                ref path,
            } if path == crate::job_tracking::JOB_STATUS_ROUTE_PATH
        ));
    }

    #[cfg(all(feature = "openapi", feature = "maud"))]
    #[tokio::test]
    async fn try_build_router_rejects_openapi_path_on_story_gallery() {
        // The story gallery merges GETs at `/_stories` (+ `/_stories/{slug}`)
        // when `stories.enabled` resolves true, before the late-merged
        // OpenAPI router, so the collision preflight must reserve it —
        // otherwise mounting OpenAPI there panics in `router.merge` instead
        // of surfacing the typed collision.
        let mut config = AutumnConfig::default();
        config.stories.enabled = true;
        let openapi = crate::openapi::OpenApiConfig::new("Demo", "1.0.0")
            .openapi_json_path(crate::stories::STORIES_PATH);
        let ctx = RouterContext {
            exception_filters: Vec::new(),
            scoped_groups: Vec::new(),
            merge_routers: Vec::new(),
            nest_routers: Vec::new(),
            declared_routes: Vec::new(),
            custom_layers: Vec::new(),
            static_gate_layers: Vec::new(),
            error_page_renderer: None,
            session_store: None,
            openapi: Some(openapi),
            #[cfg(feature = "mcp")]
            mcp: None,
        };
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("story gallery path should be reserved while stories are enabled");
        assert!(matches!(
            err,
            RouterBuildError::OpenApiPathCollision {
                field: "openapi_json_path",
                ref path,
            } if path == crate::stories::STORIES_PATH
        ));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn try_build_router_rejects_openapi_path_on_dev_live_reload() {
        temp_env::with_vars(
            [
                ("AUTUMN_DEV_RELOAD", Some("1")),
                ("AUTUMN_DEV_RELOAD_STATE", Some("/tmp/autumn-reload-test")),
            ],
            || {
                let config = AutumnConfig::default();
                let openapi = crate::openapi::OpenApiConfig::new("Demo", "1.0.0")
                    .openapi_json_path("/__autumn/live-reload");
                let ctx = RouterContext {
                    exception_filters: Vec::new(),
                    scoped_groups: Vec::new(),
                    merge_routers: Vec::new(),
                    nest_routers: Vec::new(),
                    declared_routes: Vec::new(),
                    custom_layers: Vec::new(),
                    static_gate_layers: Vec::new(),
                    error_page_renderer: None,
                    session_store: None,
                    openapi: Some(openapi),
                    #[cfg(feature = "mcp")]
                    mcp: None,
                };
                let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
                    .expect_err("dev reload path should be reserved");
                assert!(matches!(
                    err,
                    RouterBuildError::OpenApiPathCollision {
                        field: "openapi_json_path",
                        ..
                    }
                ));
            },
        );
    }

    // --- Duplicate user-route detection tests (issue #1012) ---

    async fn duplicate_route_handler() -> &'static str {
        "ok"
    }

    /// Build a lightweight [`Route`] for the duplicate-detection tests. The
    /// `MethodRouter` is built with the same HTTP method as `method` so
    /// scenarios that intentionally exercise `GET`+`POST` on the same path
    /// (AC #4) actually merge cleanly at axum level; the caller sees the
    /// duplicate-preflight decision, not an axum method-router-merge panic.
    fn duplicate_test_route(method: http::Method, path: &'static str, name: &'static str) -> Route {
        let handler = match method {
            http::Method::POST => axum::routing::post(duplicate_route_handler),
            http::Method::PUT => axum::routing::put(duplicate_route_handler),
            http::Method::PATCH => axum::routing::patch(duplicate_route_handler),
            http::Method::DELETE => axum::routing::delete(duplicate_route_handler),
            _ => axum::routing::get(duplicate_route_handler),
        };
        let method_str = if method == http::Method::POST {
            "POST"
        } else if method == http::Method::PUT {
            "PUT"
        } else if method == http::Method::PATCH {
            "PATCH"
        } else if method == http::Method::DELETE {
            "DELETE"
        } else {
            "GET"
        };
        Route {
            method,
            path,
            handler,
            name,
            api_doc: crate::openapi::ApiDoc {
                method: method_str,
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

    fn duplicate_test_ctx() -> RouterContext {
        RouterContext {
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
        }
    }

    /// AC #1, #2, #6: two routes registered on the same (method, path) fail
    /// the build with a structured [`RouterBuildError::DuplicateUserRoute`]
    /// that names both handlers and the offending method + path — no axum
    /// panic escapes.
    #[tokio::test]
    async fn try_build_router_rejects_duplicate_user_route_paths() {
        let config = AutumnConfig::default();
        let a = duplicate_test_route(http::Method::GET, "/", "root_a");
        let b = duplicate_test_route(http::Method::GET, "/", "root_b");
        let err =
            super::try_build_router_inner(vec![a, b], &config, test_state(), duplicate_test_ctx())
                .expect_err("two GET / routes should be rejected before mount");
        let display = err.to_string();
        match err {
            RouterBuildError::DuplicateUserRoute {
                ref method,
                ref path,
                ref existing,
                ref incoming,
            } => {
                assert_eq!(method, "GET");
                assert_eq!(path, "/");
                assert_eq!(existing, "root_a");
                assert_eq!(incoming, "root_b");
            }
            other => panic!("expected DuplicateUserRoute, got {other:?}"),
        }
        assert!(
            display.contains("root_a"),
            "error message must name first handler; got: {display}"
        );
        assert!(
            display.contains("root_b"),
            "error message must name second handler; got: {display}"
        );
        assert!(
            display.contains("GET"),
            "error message must name the HTTP method; got: {display}"
        );
        assert!(
            display.contains('/'),
            "error message must contain the path; got: {display}"
        );
    }

    /// A declared plugin route helper — the shape
    /// [`AppBuilder::declare_plugin_routes`](crate::app::AppBuilder::declare_plugin_routes)
    /// records, and the shape a sandboxed plugin's manifest produces.
    fn declared_route(method: &str, path: &str, plugin: &str) -> crate::route_listing::RouteInfo {
        crate::route_listing::RouteInfo {
            method: method.to_owned(),
            path: path.to_owned(),
            handler: format!("sandbox:{plugin}"),
            source: crate::route_listing::RouteSource::Plugin(plugin.to_owned()),
            ..crate::route_listing::RouteInfo::default()
        }
    }

    /// An untrusted artifact must not be able to abort the host at boot.
    ///
    /// A `nest` mount is opaque to axum, so the duplicate-route preflight
    /// historically skipped it — but a sandboxed plugin's manifest IS its route
    /// table, and `Router::nest` panics with "Overlapping method route" when a
    /// declared path is one the application already serves. That panic is a
    /// denial of service against the WHOLE app reachable from an artifact the
    /// operator was told the sandbox contains, so it has to surface as a typed
    /// error before anything mounts.
    #[tokio::test]
    async fn try_build_router_rejects_a_declared_plugin_route_the_app_already_serves() {
        let config = AutumnConfig::default();
        let app_route = duplicate_test_route(http::Method::GET, "/hello/greet", "app_greet");
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![declared_route("GET", "/hello/greet", "evil-plugin")];
        let err = super::try_build_router_inner(vec![app_route], &config, test_state(), ctx)
            .expect_err("a plugin declaring a path the app serves must be refused");
        match err {
            RouterBuildError::DuplicateUserRoute {
                ref method,
                ref path,
                ref existing,
                ref incoming,
            } => {
                assert_eq!(method, "GET");
                assert_eq!(path, "/hello/greet");
                // The application route is first-seen, so the plugin is named
                // as the incoming side — the operator needs to know WHICH
                // artifact to stop installing.
                assert_eq!(existing, "app_greet");
                assert_eq!(incoming, "sandbox:evil-plugin");
            }
            other => panic!("expected DuplicateUserRoute, got {other:?}"),
        }
    }

    /// The framework's own routes are mounted outside `route_list`, so the
    /// declared-route check has to know about them separately: a manifest
    /// declaring `GET /health` reached `Router::nest` and panicked with
    /// "Overlapping method route" even after declared routes were checked
    /// against the application's.
    ///
    /// Refused rather than yielded: a user route at a probe path legitimately
    /// takes it over (#1971), but silently handing an unaudited artifact the
    /// endpoint orchestrators read to decide whether the process is alive is a
    /// worse outcome than a loud refusal.
    #[tokio::test]
    async fn try_build_router_rejects_a_declared_plugin_route_on_a_framework_path() {
        let config = AutumnConfig::default();
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![declared_route("GET", &config.health.path, "evil-plugin")];
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("a plugin declaring a framework probe path must be refused");
        match err {
            RouterBuildError::DuplicateUserRoute {
                ref path,
                ref existing,
                ref incoming,
                ..
            } => {
                assert_eq!(path, &config.health.path);
                assert!(
                    existing.starts_with("autumn framework route")
                        && existing.contains(&config.health.path),
                    "the refusal must name the framework route it clashed with; got: {existing}"
                );
                assert_eq!(incoming, "sandbox:evil-plugin");
            }
            other => panic!("expected the framework-path refusal, got {other:?}"),
        }
    }

    /// A shape clash is method-independent, so the check must be too.
    ///
    /// matchit sits *above* method routing: two different templates at the same
    /// shape (`/_stories/{slug}` against `/_stories/{id}`) are a route conflict
    /// axum refuses whatever method the second carries. Gating the shape check
    /// on GET let a declared POST through to a startup panic — the same class
    /// the GET case already covered, reached by choosing a different verb.
    ///
    /// The exact-path half stays method-aware: `GET /health` and a declared
    /// `POST /health` merge into one `MethodRouter` and must keep working.
    #[tokio::test]
    async fn try_build_router_rejects_a_non_get_declared_route_that_shape_clashes_with_a_framework_path()
     {
        let mut config = AutumnConfig::default();
        config.health.path = "/probe/{id}".to_owned();
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![declared_route("POST", "/probe/{slug}", "evil-plugin")];
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("a shape clash must be refused whatever the method");
        match err {
            RouterBuildError::DuplicateUserRoute {
                ref method,
                ref existing,
                ..
            } => {
                assert_eq!(method, "POST", "the refusal must name the declared method");
                assert!(
                    existing.contains("/probe/{id}"),
                    "the refusal must name the framework template; got: {existing}"
                );
            }
            other => panic!("expected the framework shape refusal, got {other:?}"),
        }
    }

    /// The other half of the same rule: an EXACT path match across methods
    /// merges cleanly, so a declared POST at a framework GET path is allowed.
    #[tokio::test]
    async fn try_build_router_allows_a_non_get_declared_route_at_a_framework_get_path() {
        let config = AutumnConfig::default();
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![declared_route("POST", &config.health.path, "honest-plugin")];
        assert!(
            super::try_build_router_inner(Vec::new(), &config, test_state(), ctx).is_ok(),
            "a POST at a framework GET path merges into one MethodRouter and must be allowed"
        );
    }

    /// The framework mounts non-GET routes too, and only GET was compared.
    ///
    /// With sensitive actuator endpoints on, `PUT {prefix}/loggers/{name}` is
    /// a real mount — so a manifest declaring it passed the preflight and
    /// panicked at `Router::nest`, which is the whole class this check exists
    /// to close.
    #[tokio::test]
    async fn try_build_router_rejects_a_declared_plugin_route_on_a_framework_mutating_path() {
        let mut config = AutumnConfig::default();
        config.actuator.sensitive = true;
        let loggers =
            crate::actuator::actuator_route_path(&config.actuator.prefix, "/loggers/{name}");
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![declared_route("PUT", &loggers, "evil-plugin")];
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("a declared PUT at the actuator's own PUT must be refused");
        match err {
            RouterBuildError::DuplicateUserRoute {
                ref method,
                ref existing,
                ..
            } => {
                assert_eq!(method, "PUT", "the refusal must name the real method");
                assert!(
                    existing.contains(&loggers),
                    "the refusal must name the framework route; got: {existing}"
                );
            }
            other => panic!("expected the framework mutating-route refusal, got {other:?}"),
        }
    }

    /// The other direction: a path the framework mounts only as POST must not
    /// be claimed as a GET.
    ///
    /// `{prefix}/webhooks/replay` is in `actuator_endpoint_paths` solely to
    /// seed the startup barrier (#1627), and it is mounted POST-only. GET and
    /// that POST merge into one `MethodRouter` cleanly, so refusing a declared
    /// GET there rejects a mount axum accepts — the failure mode this check
    /// has to avoid as much as the panic it prevents.
    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn try_build_router_allows_a_declared_get_at_a_post_only_framework_path() {
        let mut config = AutumnConfig::default();
        config.actuator.sensitive = true;
        let replay =
            crate::actuator::actuator_route_path(&config.actuator.prefix, "/webhooks/replay");
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![declared_route("GET", &replay, "honest-plugin")];
        assert!(
            super::try_build_router_inner(Vec::new(), &config, test_state(), ctx).is_ok(),
            "a GET at a POST-only framework path merges cleanly and must be allowed"
        );
    }

    /// A framework path that carries a capture conflicts with a differently
    /// NAMED capture at the same position — axum refuses that shape whatever
    /// the method — so the framework comparison has to go through matchit, not
    /// string equality. `/_stories/{slug}` is the live example; the
    /// operator-configurable paths (probes, actuator prefix, dev inspector,
    /// job status) can carry captures for the same reason.
    #[tokio::test]
    async fn try_build_router_rejects_a_declared_plugin_route_that_shape_clashes_with_a_framework_path()
     {
        let mut config = AutumnConfig::default();
        config.health.path = "/probe/{id}".to_owned();
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![declared_route("GET", "/probe/{slug}", "evil-plugin")];
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("a shape clash with a framework path must be refused");
        match err {
            RouterBuildError::DuplicateUserRoute { ref existing, .. } => assert!(
                existing.contains("/probe/{id}"),
                "the refusal must name the framework template it clashed with; got: {existing}"
            ),
            other => panic!("expected the framework shape refusal, got {other:?}"),
        }
    }

    /// The dev inspector mounts TWO routes — an index at the configured path
    /// and a detail template below it — so claiming only the configured path
    /// left `{inspector_path}/requests/{id}` open. It is invisible while the
    /// inspector sits under the reserved `/_autumn` root; move it anywhere
    /// else, as `dev.inspector_path` invites, and a plugin declaring that
    /// shape reaches `Router::merge` and panics.
    #[tokio::test]
    async fn try_build_router_rejects_a_declared_plugin_route_on_the_inspector_detail_path() {
        let mut config = AutumnConfig {
            profile: Some("dev".to_owned()),
            ..AutumnConfig::default()
        };
        config.dev.inspector_path = "/debug".to_owned();
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![declared_route(
            "GET",
            "/debug/requests/{slug}",
            "evil-plugin",
        )];
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("the inspector detail route must be claimed too");
        match err {
            RouterBuildError::DuplicateUserRoute { ref existing, .. } => assert!(
                existing.contains("/debug/requests/{id}"),
                "the refusal must name the inspector detail template; got: {existing}"
            ),
            other => panic!("expected the inspector detail refusal, got {other:?}"),
        }
    }

    /// Only `GET` actually clashes at a framework path — axum merges a declared
    /// `HEAD` or `POST` into the same `MethodRouter` without complaint
    /// (verified against axum 0.8.9). Refusing those would reject mounts axum
    /// accepts, so the check must stay method-aware.
    #[tokio::test]
    async fn try_build_router_allows_a_declared_plugin_post_on_a_framework_path() {
        let config = AutumnConfig::default();
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![
            declared_route("POST", &config.health.path, "good-plugin"),
            declared_route("HEAD", &config.health.path, "good-plugin"),
        ];
        let _router = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect("non-GET declarations at a framework path must mount");
    }

    /// `/static` and `/_autumn` are framework namespaces, not enumerable
    /// routes: `ServeDir` serves whatever is on disk, so no exact-path claim
    /// can cover them. A plugin declaring a sub-path there does not even panic
    /// — it mounts and SHADOWS the framework, which for `/static/app.js` means
    /// an unaudited artifact serving script from the host's own origin.
    /// Refused for every method, at the namespace root and below.
    #[tokio::test]
    async fn try_build_router_rejects_declared_plugin_routes_inside_framework_namespaces() {
        for (method, path) in [
            ("GET", "/static/app.js"),
            ("GET", "/static"),
            ("POST", "/_autumn/unsubscribe"),
            ("GET", "/_autumn/jobs/abc"),
        ] {
            let config = AutumnConfig::default();
            let mut ctx = duplicate_test_ctx();
            ctx.declared_routes = vec![declared_route(method, path, "evil-plugin")];
            let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
                .expect_err("a plugin route inside a framework namespace must be refused");
            match err {
                RouterBuildError::DuplicateUserRoute {
                    path: ref got,
                    ref existing,
                    ..
                } => {
                    assert_eq!(got, path);
                    assert!(
                        existing.contains("framework namespace"),
                        "the refusal must name the namespace; got: {existing}"
                    );
                }
                other => {
                    panic!("expected the namespace refusal for {method} {path}, got {other:?}")
                }
            }
        }
    }

    /// The namespace check matches on segment boundaries, not string prefixes:
    /// `/staticky` is an ordinary path a plugin may legitimately serve.
    #[tokio::test]
    async fn try_build_router_allows_a_plugin_path_that_merely_starts_like_a_namespace() {
        let config = AutumnConfig::default();
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![declared_route("GET", "/staticky/thing", "good-plugin")];
        let _router = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect("/staticky must not be read as /static");
    }

    /// `mount_probe_endpoints` mounts nothing when `health.enabled = false`, so
    /// claiming the probe paths anyway would refuse a plugin over a collision
    /// that cannot happen — turning a working install into a startup failure.
    #[tokio::test]
    async fn try_build_router_allows_a_declared_probe_path_when_probes_are_disabled() {
        let mut config = AutumnConfig::default();
        config.health.enabled = false;
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![declared_route("GET", &config.health.path, "good-plugin")];
        let _router = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect("a disabled probe path must not be claimed");
    }

    /// The same protection has to cover a *shape* clash, not just an exact
    /// path: `axum::Router::nest` panics identically when a declared
    /// `/hello/{slug}` meets an application `/hello/{id}` (matchit rejects the
    /// second template regardless of method).
    #[tokio::test]
    async fn try_build_router_rejects_a_declared_plugin_route_that_shape_clashes() {
        let config = AutumnConfig::default();
        let app_route = duplicate_test_route(http::Method::GET, "/hello/{id}", "app_show");
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![declared_route("POST", "/hello/{slug}", "evil-plugin")];
        let err = super::try_build_router_inner(vec![app_route], &config, test_state(), ctx)
            .expect_err("a shape clash between app and plugin routes must be refused");
        assert!(
            matches!(err, RouterBuildError::ConflictingRouteShape { .. }),
            "expected ConflictingRouteShape, got {err:?}"
        );
    }

    /// The check must not fire on mounts axum accepts, or every plugin install
    /// becomes a startup failure. Disjoint paths under a shared prefix, and a
    /// plugin route sitting AT a path the app serves as a *parent*, both mount
    /// cleanly — verified against axum itself before this test was written.
    #[tokio::test]
    async fn try_build_router_allows_declared_plugin_routes_that_do_not_collide() {
        let config = AutumnConfig::default();
        let routes = vec![
            duplicate_test_route(http::Method::GET, "/hello/other", "app_other"),
            duplicate_test_route(http::Method::GET, "/hello", "app_index"),
        ];
        let mut ctx = duplicate_test_ctx();
        ctx.declared_routes = vec![
            declared_route("GET", "/hello/greet", "good-plugin"),
            // A GET route's implied HEAD is declared alongside it; the two
            // share a path and must not be read as a duplicate of each other.
            declared_route("HEAD", "/hello/greet", "good-plugin"),
        ];
        let _router = super::try_build_router_inner(routes, &config, test_state(), ctx)
            .expect("non-colliding plugin routes must mount");
    }

    /// AC #4: distinct methods on the same path (`GET /admin` + `POST /admin`)
    /// must NOT be flagged — axum merges them cleanly into a single
    /// `MethodRouter`.
    #[tokio::test]
    async fn try_build_router_allows_distinct_methods_on_same_path() {
        let config = AutumnConfig::default();
        let get = duplicate_test_route(http::Method::GET, "/admin", "admin_index");
        let post = duplicate_test_route(http::Method::POST, "/admin", "admin_create");
        let _router = super::try_build_router_inner(
            vec![get, post],
            &config,
            test_state(),
            duplicate_test_ctx(),
        )
        .expect("GET + POST on the same path should build cleanly");
    }

    /// AC #3: duplicates that span a top-level route and a scoped group
    /// (once the scope prefix is applied) are detected using the same
    /// preflight — the introspection reuses `RouteInfo`'s scope resolution.
    #[tokio::test]
    async fn try_build_router_rejects_duplicate_across_scoped_group() {
        let config = AutumnConfig::default();
        let top = duplicate_test_route(http::Method::GET, "/api/posts", "top_posts");
        let scoped_child = duplicate_test_route(http::Method::GET, "/posts", "scoped_posts");
        let group = crate::app::ScopedGroup {
            prefix: "/api".to_owned(),
            routes: vec![scoped_child],
            source: crate::route_listing::RouteSource::User,
            apply_layer: Box::new(|r| r),
        };
        let mut ctx = duplicate_test_ctx();
        ctx.scoped_groups.push(group);
        let err = super::try_build_router_inner(vec![top], &config, test_state(), ctx)
            .expect_err("top-level + scoped resolving to same path should be rejected");
        match err {
            RouterBuildError::DuplicateUserRoute {
                ref method,
                ref path,
                ..
            } => {
                assert_eq!(method, "GET");
                assert_eq!(path, "/api/posts");
            }
            other => panic!("expected DuplicateUserRoute, got {other:?}"),
        }
    }

    /// AC #3: two scoped groups whose resolved paths collide are also
    /// caught (a plugin re-registering the same route class as user code).
    #[tokio::test]
    async fn try_build_router_rejects_duplicate_within_scoped_groups() {
        let config = AutumnConfig::default();
        let a = crate::app::ScopedGroup {
            prefix: "/api".to_owned(),
            routes: vec![duplicate_test_route(
                http::Method::GET,
                "/posts",
                "user_posts",
            )],
            source: crate::route_listing::RouteSource::User,
            apply_layer: Box::new(|r| r),
        };
        let b = crate::app::ScopedGroup {
            prefix: "/api".to_owned(),
            routes: vec![duplicate_test_route(
                http::Method::GET,
                "/posts",
                "plugin_posts",
            )],
            source: crate::route_listing::RouteSource::Plugin("blog".to_owned()),
            apply_layer: Box::new(|r| r),
        };
        let mut ctx = duplicate_test_ctx();
        ctx.scoped_groups.push(a);
        ctx.scoped_groups.push(b);
        let err = super::try_build_router_inner(Vec::new(), &config, test_state(), ctx)
            .expect_err("two scoped groups colliding on /api/posts should be rejected");
        assert!(matches!(
            err,
            RouterBuildError::DuplicateUserRoute { ref existing, ref incoming, .. }
                if existing == "user_posts" && incoming == "plugin_posts"
        ));
    }

    /// AC #5: an opaque `AppBuilder::merge` router coexisting with a clean
    /// route table must not cause a false-pass failure — the check is
    /// skipped (with the existing "check skipped" warning) and the build
    /// continues. Regression guard for the collision preflight.
    #[tokio::test]
    async fn try_build_router_skips_duplicate_check_for_opaque_merge_router() {
        let config = AutumnConfig::default();
        let ok_route = duplicate_test_route(http::Method::GET, "/hello", "hello");
        let raw = axum::Router::<AppState>::new()
            .route("/raw", axum::routing::get(duplicate_route_handler));
        let mut ctx = duplicate_test_ctx();
        ctx.merge_routers.push(raw);
        let _router = super::try_build_router_inner(vec![ok_route], &config, test_state(), ctx)
            .expect("opaque merge routers must not fail the duplicate preflight");
    }

    /// AC #5: same regression guard for opaque `AppBuilder::nest` routers.
    #[tokio::test]
    async fn try_build_router_skips_duplicate_check_for_opaque_nest_router() {
        let config = AutumnConfig::default();
        let ok_route = duplicate_test_route(http::Method::GET, "/hello", "hello");
        let nested = axum::Router::<AppState>::new()
            .route("/child", axum::routing::get(duplicate_route_handler));
        let mut ctx = duplicate_test_ctx();
        ctx.nest_routers.push(("/plugin".to_owned(), nested));
        let _router = super::try_build_router_inner(vec![ok_route], &config, test_state(), ctx)
            .expect("opaque nest routers must not fail the duplicate preflight");
    }

    /// Finding 1 (issue #1012 review): two handlers that differ ONLY by capture
    /// name — `/users/{id}` vs `/users/{slug}` — key by literal template so they
    /// look distinct to a naive preflight, but axum's matcher (verified against
    /// axum 0.8.9: matchit reports a "conflict") rejects the second route shape
    /// at mount. Because the two EXACT templates differ, this is a matchit route
    /// conflict (illegal regardless of method), surfaced as
    /// `ConflictingRouteShape` naming both handlers AND both original templates.
    #[tokio::test]
    async fn try_build_router_rejects_duplicate_capture_name_paths() {
        let config = AutumnConfig::default();
        let a = duplicate_test_route(http::Method::GET, "/users/{id}", "by_id");
        let b = duplicate_test_route(http::Method::GET, "/users/{slug}", "by_slug");
        let err =
            super::try_build_router_inner(vec![a, b], &config, test_state(), duplicate_test_ctx())
                .expect_err("capture-name-only difference must be rejected before mount");
        match err {
            RouterBuildError::ConflictingRouteShape {
                ref existing,
                ref existing_path,
                ref incoming,
                ref incoming_path,
            } => {
                assert_eq!(existing, "by_id");
                assert_eq!(existing_path, "/users/{id}");
                assert_eq!(incoming, "by_slug");
                assert_eq!(incoming_path, "/users/{slug}");
            }
            other => panic!("expected ConflictingRouteShape, got {other:?}"),
        }
        // The diagnostic must name BOTH original templates, not the normalized key.
        let display = err.to_string();
        assert!(
            display.contains("/users/{id}") && display.contains("/users/{slug}"),
            "error must show both original path templates; got: {display}"
        );
    }

    /// Finding 1, scoped-group variant: the capture-name normalization must run
    /// AFTER `join_nested_path` prefix resolution, so a scoped `/users/{slug}`
    /// under `/api` collides with a top-level `/api/users/{id}`.
    #[tokio::test]
    async fn try_build_router_rejects_duplicate_capture_name_across_scoped_group() {
        let config = AutumnConfig::default();
        let top = duplicate_test_route(http::Method::GET, "/api/users/{id}", "top_by_id");
        let scoped_child =
            duplicate_test_route(http::Method::GET, "/users/{slug}", "scoped_by_slug");
        let group = crate::app::ScopedGroup {
            prefix: "/api".to_owned(),
            routes: vec![scoped_child],
            source: crate::route_listing::RouteSource::User,
            apply_layer: Box::new(|r| r),
        };
        let mut ctx = duplicate_test_ctx();
        ctx.scoped_groups.push(group);
        let err = super::try_build_router_inner(vec![top], &config, test_state(), ctx)
            .expect_err("scoped capture-name collision must be rejected before mount");
        assert!(
            matches!(
                err,
                RouterBuildError::ConflictingRouteShape {
                    ref existing, ref incoming, ref existing_path, ref incoming_path
                }
                    if existing == "top_by_id" && incoming == "scoped_by_slug"
                        && existing_path == "/api/users/{id}"
                        && incoming_path == "/api/users/{slug}"
            ),
            "expected ConflictingRouteShape naming both handlers + both paths, got {err:?}"
        );
    }

    /// Finding 1 NEGATIVE guard: normalization must not over-flag. Two genuinely
    /// different shapes that axum's matcher accepts (verified: `/users/{id}` and
    /// `/users/{id}/posts` do NOT conflict) must still build cleanly.
    #[tokio::test]
    async fn try_build_router_allows_distinct_route_shapes() {
        let config = AutumnConfig::default();
        let a = duplicate_test_route(http::Method::GET, "/users/{id}", "show");
        let b = duplicate_test_route(http::Method::GET, "/users/{id}/posts", "posts");
        let _router =
            super::try_build_router_inner(vec![a, b], &config, test_state(), duplicate_test_ctx())
                .expect("distinct route shapes must not be flagged as duplicates");
    }

    /// Finding 2 (issue #1012 review): `#[ws]` records the synthetic `WS` method
    /// but `group_and_mount_routes` mounts its handler via `axum::routing::get`,
    /// so `#[get("/live")]` + `#[ws("/live")]` produce two overlapping `GET`
    /// `MethodRouter`s that panic on merge. Normalizing `WS` to its effective
    /// `GET` before keying makes the preflight catch it as `DuplicateUserRoute`.
    ///
    /// Not `#[cfg(feature = "ws")]`-gated: the synthetic `WS` method is a plain
    /// `http::Method` string, and the sibling `build_route_timeout_table_*` test
    /// exercises the same normalization ungated — gating would hide this from the
    /// default `cargo test` run since `ws` is not a default feature.
    #[tokio::test]
    async fn try_build_router_rejects_ws_get_collision() {
        let config = AutumnConfig::default();
        let get = duplicate_test_route(http::Method::GET, "/live", "live_poll");
        let ws = duplicate_test_route(
            http::Method::from_bytes(b"WS").unwrap(),
            "/live",
            "live_socket",
        );
        let err = super::try_build_router_inner(
            vec![get, ws],
            &config,
            test_state(),
            duplicate_test_ctx(),
        )
        .expect_err("GET + WS on the same path must be rejected before mount");
        match err {
            RouterBuildError::DuplicateUserRoute {
                ref method,
                ref path,
                ref existing,
                ref incoming,
            } => {
                assert_eq!(method, "GET", "WS must be normalized to its effective GET");
                assert_eq!(path, "/live");
                assert_eq!(existing, "live_poll");
                assert_eq!(incoming, "live_socket");
            }
            other => panic!("expected DuplicateUserRoute, got {other:?}"),
        }
    }

    /// Finding A (round 2): different HTTP methods whose paths differ ONLY by
    /// capture name (`GET /users/{id}` + `POST /users/{slug}`) key as distinct
    /// `(method, shape)` pairs, so the method-independent shape check must catch
    /// them. Verified against axum 0.8.9: `Router::route("/users/{id}", get)`
    /// then `Router::route("/users/{slug}", post)` PANICS — matchit rejects the
    /// second template as a route conflict BEFORE method merging. The preflight
    /// surfaces it as `ConflictingRouteShape` naming both handlers + both
    /// templates; no axum panic escapes.
    #[tokio::test]
    async fn try_build_router_rejects_cross_method_shape_conflict() {
        let config = AutumnConfig::default();
        let get = duplicate_test_route(http::Method::GET, "/users/{id}", "by_id");
        let post = duplicate_test_route(http::Method::POST, "/users/{slug}", "by_slug");
        let err = super::try_build_router_inner(
            vec![get, post],
            &config,
            test_state(),
            duplicate_test_ctx(),
        )
        .expect_err("cross-method capture-name-only conflict must be rejected before mount");
        match err {
            RouterBuildError::ConflictingRouteShape {
                ref existing,
                ref existing_path,
                ref incoming,
                ref incoming_path,
            } => {
                assert_eq!(existing, "by_id");
                assert_eq!(existing_path, "/users/{id}");
                assert_eq!(incoming, "by_slug");
                assert_eq!(incoming_path, "/users/{slug}");
            }
            other => panic!("expected ConflictingRouteShape, got {other:?}"),
        }
        let display = err.to_string();
        assert!(
            display.contains("by_id") && display.contains("by_slug"),
            "error must name both handlers; got: {display}"
        );
        assert!(
            display.contains("/users/{id}") && display.contains("/users/{slug}"),
            "error must name both original templates; got: {display}"
        );
    }

    /// Finding A, scoped-group variant: the method-independent shape conflict
    /// check must run AFTER `join_nested_path`, so a scoped `POST /users/{slug}`
    /// under `/api` conflicts with a top-level `GET /api/users/{id}`.
    #[tokio::test]
    async fn try_build_router_rejects_cross_method_shape_conflict_across_scoped_group() {
        let config = AutumnConfig::default();
        let top = duplicate_test_route(http::Method::GET, "/api/users/{id}", "top_by_id");
        let scoped_child =
            duplicate_test_route(http::Method::POST, "/users/{slug}", "scoped_by_slug");
        let group = crate::app::ScopedGroup {
            prefix: "/api".to_owned(),
            routes: vec![scoped_child],
            source: crate::route_listing::RouteSource::User,
            apply_layer: Box::new(|r| r),
        };
        let mut ctx = duplicate_test_ctx();
        ctx.scoped_groups.push(group);
        let err = super::try_build_router_inner(vec![top], &config, test_state(), ctx)
            .expect_err("scoped cross-method shape conflict must be rejected before mount");
        assert!(
            matches!(
                err,
                RouterBuildError::ConflictingRouteShape {
                    ref existing, ref incoming, ref existing_path, ref incoming_path
                }
                    if existing == "top_by_id" && incoming == "scoped_by_slug"
                        && existing_path == "/api/users/{id}"
                        && incoming_path == "/api/users/{slug}"
            ),
            "expected ConflictingRouteShape naming both handlers + both paths, got {err:?}"
        );
    }

    /// AC #4 (round 2): the SAME exact capture template on distinct methods
    /// (`GET /users/{id}` + `POST /users/{id}`) is LEGAL — axum merges the two
    /// `MethodRouter`s. Verified against axum 0.8.9: this pair builds cleanly.
    /// The shape check keys on the FIRST exact template per shape, so an
    /// identical template never trips it — only a DIFFERENT template does.
    #[tokio::test]
    async fn try_build_router_allows_same_capture_template_distinct_methods() {
        let config = AutumnConfig::default();
        let get = duplicate_test_route(http::Method::GET, "/users/{id}", "show");
        let post = duplicate_test_route(http::Method::POST, "/users/{id}", "update");
        let _router = super::try_build_router_inner(
            vec![get, post],
            &config,
            test_state(),
            duplicate_test_ctx(),
        )
        .expect("same capture template on GET + POST must build cleanly");
    }

    /// Finding B (round 2): axum/matchit treat `{{`/`}}` as ESCAPED literal
    /// braces, so `/{{foo}}` and `/{{bar}}` are two DISTINCT static routes.
    /// Verified against axum 0.8.9: both build cleanly. The matchit oracle
    /// treats escaped braces as literals (not captures), so this valid app is
    /// not falsely rejected.
    #[tokio::test]
    async fn try_build_router_allows_escaped_brace_literals() {
        let config = AutumnConfig::default();
        let a = duplicate_test_route(http::Method::GET, "/{{foo}}", "lit_foo");
        let b = duplicate_test_route(http::Method::GET, "/{{bar}}", "lit_bar");
        let _router =
            super::try_build_router_inner(vec![a, b], &config, test_state(), duplicate_test_ctx())
                .expect("distinct escaped-literal paths must not be flagged as duplicates");
    }

    /// Finding B guard: an escaped-literal prefix combined with a real capture
    /// keeps the shapes distinct — `/{{x}}/{id}` and `/{{y}}/{id}` differ only in
    /// their literal segment. Verified against axum 0.8.9: both build cleanly.
    #[tokio::test]
    async fn try_build_router_allows_escaped_literal_prefix_with_capture() {
        let config = AutumnConfig::default();
        let a = duplicate_test_route(http::Method::GET, "/{{x}}/{id}", "x_show");
        let b = duplicate_test_route(http::Method::GET, "/{{y}}/{id}", "y_show");
        let _router =
            super::try_build_router_inner(vec![a, b], &config, test_state(), duplicate_test_ctx())
                .expect("distinct escaped-literal prefixes with a shared capture must build");
    }

    /// Adversarial sweep: a mixed literal+capture segment must normalize at the
    /// char level. `/file.{ext}` and `/file.{kind}` share the shape `/file.{…}`.
    /// Verified against axum 0.8.9: this pair PANICS (matchit conflict), so the
    /// preflight must flag it as `ConflictingRouteShape` naming both templates.
    #[tokio::test]
    async fn try_build_router_rejects_mixed_literal_capture_shape_conflict() {
        let config = AutumnConfig::default();
        let a = duplicate_test_route(http::Method::GET, "/file.{ext}", "by_ext");
        let b = duplicate_test_route(http::Method::GET, "/file.{kind}", "by_kind");
        let err =
            super::try_build_router_inner(vec![a, b], &config, test_state(), duplicate_test_ctx())
                .expect_err("mixed literal+capture shape conflict must be rejected before mount");
        assert!(
            matches!(
                err,
                RouterBuildError::ConflictingRouteShape {
                    ref existing_path, ref incoming_path, ..
                }
                    if existing_path == "/file.{ext}" && incoming_path == "/file.{kind}"
            ),
            "expected ConflictingRouteShape naming both templates, got {err:?}"
        );
    }

    /// Adversarial sweep NEGATIVE guard: a mixed literal+capture segment stays
    /// distinct from a fully static segment. `/file.{ext}` and `/file.json` do
    /// NOT share a shape. Verified against axum 0.8.9: this pair builds cleanly,
    /// so the char-level normalization must not over-collapse the static one.
    #[tokio::test]
    async fn try_build_router_allows_mixed_capture_vs_static_segment() {
        let config = AutumnConfig::default();
        let a = duplicate_test_route(http::Method::GET, "/file.{ext}", "by_ext");
        let b = duplicate_test_route(http::Method::GET, "/file.json", "static_json");
        let _router =
            super::try_build_router_inner(vec![a, b], &config, test_state(), duplicate_test_ctx())
                .expect("a capture segment and a static segment must not be flagged as duplicates");
    }

    /// Adversarial sweep: a normal capture and a catch-all at the same terminal
    /// position (`/u/{id}` vs `/u/{*rest}`) collapse to the same placeholder.
    /// Verified against axum 0.8.9: this pair PANICS (matchit conflict), so the
    /// preflight must flag it. Different exact templates → `ConflictingRouteShape`.
    #[tokio::test]
    async fn try_build_router_rejects_catch_all_vs_normal_capture() {
        let config = AutumnConfig::default();
        let a = duplicate_test_route(http::Method::GET, "/u/{id}", "one");
        let b = duplicate_test_route(http::Method::GET, "/u/{*rest}", "rest");
        let err =
            super::try_build_router_inner(vec![a, b], &config, test_state(), duplicate_test_ctx())
                .expect_err("catch-all vs normal capture must be rejected before mount");
        assert!(
            matches!(
                err,
                RouterBuildError::ConflictingRouteShape {
                    ref existing_path, ref incoming_path, ..
                }
                    if existing_path == "/u/{id}" && incoming_path == "/u/{*rest}"
            ),
            "expected ConflictingRouteShape naming both templates, got {err:?}"
        );
    }

    /// New #1012 finding (matchit oracle): a catch-all conflicts with a dynamic
    /// DESCENDANT, not just a sibling capture. `GET /cmd/{tool}/{sub}` +
    /// `POST /cmd/{*path}` slipped past the old hand-rolled shape normalizer
    /// (which only unified captures position-by-position) and axum still panicked
    /// at mount. Delegating to matchit — the engine axum uses — catches it:
    /// verified against axum 0.8.9 that this pair PANICS. The preflight surfaces
    /// it as `ConflictingRouteShape` naming both handlers + both templates; no
    /// axum/matchit panic escapes.
    #[tokio::test]
    async fn try_build_router_rejects_catch_all_vs_dynamic_descendant() {
        let config = AutumnConfig::default();
        let a = duplicate_test_route(http::Method::GET, "/cmd/{tool}/{sub}", "cmd_sub");
        let b = duplicate_test_route(http::Method::POST, "/cmd/{*path}", "cmd_all");
        let err =
            super::try_build_router_inner(vec![a, b], &config, test_state(), duplicate_test_ctx())
                .expect_err("catch-all vs dynamic descendant must be rejected before mount");
        match err {
            RouterBuildError::ConflictingRouteShape {
                ref existing,
                ref existing_path,
                ref incoming,
                ref incoming_path,
            } => {
                assert_eq!(existing, "cmd_sub");
                assert_eq!(existing_path, "/cmd/{tool}/{sub}");
                assert_eq!(incoming, "cmd_all");
                assert_eq!(incoming_path, "/cmd/{*path}");
            }
            other => panic!("expected ConflictingRouteShape, got {other:?}"),
        }
        let display = err.to_string();
        assert!(
            display.contains("/cmd/{tool}/{sub}") && display.contains("/cmd/{*path}"),
            "error must name both original templates; got: {display}"
        );
    }

    /// Negative regression guard for the matchit oracle: a STATIC segment and a
    /// dynamic capture at the same position (`/users/me` + `/users/{id}`) do NOT
    /// conflict — matchit (and thus axum 0.8.9) accepts both, matching static
    /// before dynamic. The oracle must NOT raise a false positive here. Confirmed
    /// by `matchit_agrees_with_axum_route_conflicts`.
    #[tokio::test]
    async fn try_build_router_allows_static_vs_dynamic_segment() {
        let config = AutumnConfig::default();
        let a = duplicate_test_route(http::Method::GET, "/users/me", "me");
        let b = duplicate_test_route(http::Method::GET, "/users/{id}", "by_id");
        let _router =
            super::try_build_router_inner(vec![a, b], &config, test_state(), duplicate_test_ctx())
                .expect("a static segment and a dynamic capture must not be flagged as a conflict");
    }

    /// Parity guard for the #1012 matchit oracle: matchit's `insert` Ok/Err MUST
    /// agree with axum 0.8.9's `Router::route` accept/panic on every case of the
    /// conflict matrix. axum wraps matchit, so they should always agree — this
    /// test fails LOUDLY if a future axum bump (or a `matchit` version that
    /// drifts out of lockstep with the `=0.8.4` pin) changes conflict semantics,
    /// catching silent oracle divergence before it can introduce false
    /// positives/negatives at mount. Deterministic and fast (no async, no I/O).
    #[test]
    fn matchit_agrees_with_axum_route_conflicts() {
        // (template_a, template_b, expect_conflict)
        let matrix: &[(&str, &str, bool)] = &[
            ("/users/{id}", "/users/{slug}", true),
            ("/users/{id}", "/users/{id}/posts", false),
            ("/cmd/{tool}/{sub}", "/cmd/{*path}", true),
            ("/users/me", "/users/{id}", false),
            ("/{{foo}}", "/{{bar}}", false),
            ("/file.{ext}", "/file.{kind}", true),
            ("/file.{ext}", "/file.json", false),
            ("/u/{id}", "/u/{*rest}", true),
        ];

        // Silence axum/matchit's panic backtrace noise while we intentionally
        // trip conflicts under catch_unwind; restore the hook afterwards.
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let mut rows = Vec::new();
        let mut mismatches = Vec::new();
        for &(a, b, expect_conflict) in matrix {
            // axum: does registering BOTH templates panic inside `route()`?
            let axum_panics = std::panic::catch_unwind(|| {
                let _ = axum::Router::<()>::new()
                    .route(a, axum::routing::get(|| async { "a" }))
                    .route(b, axum::routing::get(|| async { "b" }));
            })
            .is_err();

            // matchit: does inserting BOTH templates report a conflict?
            let mut r: matchit::Router<()> = matchit::Router::new();
            r.insert(a, ()).expect("first template must insert cleanly");
            let matchit_conflicts =
                matches!(r.insert(b, ()), Err(matchit::InsertError::Conflict { .. }));

            rows.push(format!(
                "{a:<20} vs {b:<20} axum={} matchit={} expected={}",
                if axum_panics { "PANIC" } else { "ok" },
                if matchit_conflicts { "Err" } else { "Ok" },
                if expect_conflict { "conflict" } else { "ok" },
            ));

            if axum_panics != matchit_conflicts || axum_panics != expect_conflict {
                mismatches.push(rows.last().unwrap().clone());
            }
        }

        std::panic::set_hook(prev_hook);

        assert!(
            mismatches.is_empty(),
            "matchit must agree with axum 0.8.9 AND the expected outcome on every \
             case (oracle divergence => false positives/negatives at mount).\n\
             full matrix:\n{}\nmismatches:\n{}",
            rows.join("\n"),
            mismatches.join("\n"),
        );
    }

    // --- Static file serving (SSG/ISG) tests ---

    // --- SSG/ISG response compression (#752) ---
    //
    // The static-first middleware serves manifest-backed `dist/` files from disk
    // and short-circuits before the dynamic router. Compression is applied
    // outside that middleware (see `try_build_router_with_static_inner`), and the
    // served response carries a MIME type derived from the file extension, so the
    // compression layer encodes compressible SSG pages and leaves binary assets
    // alone — matching dynamic handler responses.

    /// Build a `dist/` dir + `manifest.json` mapping each `(route, file, bytes)`
    /// tuple to a file on disk. Returns the `TempDir` guard; the dist directory
    /// is at `<tmp>/dist`.
    fn create_ssg_dist(entries: &[(&str, &str, &[u8])]) -> tempfile::TempDir {
        let with_types: Vec<_> = entries
            .iter()
            .map(|(route, file, bytes)| (*route, *file, *bytes, None))
            .collect();
        create_ssg_dist_with_types(&with_types)
    }

    /// Like [`create_ssg_dist`], but each entry also carries the `Content-Type`
    /// recorded in the manifest at generation time (#1832). `None` reproduces a
    /// pre-#1832 (or hand-written) manifest that records nothing.
    fn create_ssg_dist_with_types(
        entries: &[(&str, &str, &[u8], Option<&str>)],
    ) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let dist = dir.path().join("dist");
        let mut routes = std::collections::HashMap::new();
        for (route, file, bytes, content_type) in entries {
            let path = dist.join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(&path, bytes).expect("write file");
            routes.insert(
                (*route).to_owned(),
                crate::static_gen::ManifestEntry::new(*file)
                    .with_content_type(content_type.map(str::to_owned)),
            );
        }
        let manifest = crate::static_gen::StaticManifest {
            generated_at: "2026-07-12T00:00:00Z".to_owned(),
            autumn_version: "0.6.0".to_owned(),
            routes,
        };
        std::fs::write(
            dist.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        dir
    }

    fn compression_enabled_config() -> AutumnConfig {
        let mut config = AutumnConfig::default();
        config.compression.enabled = true;
        config
    }

    /// A manifest-backed HTML page is gzip-compressed when the client accepts
    /// gzip and framework compression is enabled, and it carries
    /// `Vary: Accept-Encoding`.
    #[tokio::test]
    async fn ssg_html_hit_is_gzip_compressed() {
        let html = format!(
            "<html><body>{}</body></html>",
            "Lorem ipsum dolor sit amet. ".repeat(64)
        );
        let tmp = create_ssg_dist(&[("/", "index.html", html.as_bytes())]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "manifest-backed SSG HTML page must be gzip-compressed"
        );
        let vary = response
            .headers()
            .get(http::header::VARY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            vary.to_lowercase().contains("accept-encoding"),
            "Vary must advertise Accept-Encoding, got {vary:?}"
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8"),
            "HTML page keeps its text/html content type"
        );
        // The transferred body is the gzip stream, not the raw HTML.
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_ne!(
            body.as_ref(),
            html.as_bytes(),
            "compressed body must differ from the raw HTML"
        );
    }

    /// A manifest-backed *binary* asset is served with its real MIME type and is
    /// NOT compressed, even though the client accepts gzip — proving the fix
    /// does not blindly compress non-text responses.
    #[tokio::test]
    async fn ssg_binary_asset_is_not_compressed_and_keeps_mime() {
        // PNG signature followed by pseudo-random bytes, padded well past the
        // compression size floor so size is not the reason it is skipped.
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend((0u32..1024).map(|i| i.wrapping_mul(2_654_435_761).to_le_bytes()[0]));
        let tmp = create_ssg_dist(&[("/logo", "logo.png", &bytes)]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/logo")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("image/png"),
            "binary manifest asset must keep its real MIME type, not text/html"
        );
        assert_eq!(
            response.headers().get(http::header::CONTENT_ENCODING),
            None,
            "binary asset must not be blindly compressed"
        );
    }

    /// A manifest-backed pre-compressed web font (`.woff2`) is served with its
    /// `font/woff2` MIME type and is NOT gzip-compressed even though the client
    /// accepts gzip: WOFF/WOFF2 embed their own compression, so re-encoding only
    /// wastes CPU. Raw fonts (`.ttf`/`.otf`) are deliberately left compressible.
    #[tokio::test]
    async fn ssg_woff2_font_is_not_compressed_and_keeps_mime() {
        // Pad well past the compression size floor so size is not the reason it
        // is skipped — the font MIME type is.
        let mut bytes = b"wOF2".to_vec();
        bytes.extend((0u32..1024).map(|i| i.wrapping_mul(2_654_435_761).to_le_bytes()[0]));
        let tmp = create_ssg_dist(&[("/inter", "fonts/inter.woff2", &bytes)]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/inter")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("font/woff2"),
            "woff2 manifest asset must keep its font/woff2 MIME type"
        );
        assert_eq!(
            response.headers().get(http::header::CONTENT_ENCODING),
            None,
            "pre-compressed woff2 font must not be re-compressed"
        );
    }

    /// The other half of the two-entry font carve-out: WOFF **v1** is
    /// pre-compressed too, and `COMPRESSION_EXCLUDED_CONTENT_TYPES` lists
    /// `font/woff` separately from `font/woff2`. Without this, deleting the
    /// `font/woff` line leaves the suite green.
    #[tokio::test]
    async fn ssg_woff_font_is_not_compressed_and_keeps_mime() {
        let mut bytes = b"wOFF".to_vec();
        bytes.extend((0u32..1024).map(|i| i.wrapping_mul(2_654_435_761).to_le_bytes()[0]));

        // Both paths: the legacy derivation from the file name, and a manifest
        // that recorded `font/woff` at generation time.
        for (label, recorded) in [("derived", None), ("recorded", Some("font/woff"))] {
            let tmp =
                create_ssg_dist_with_types(&[("/inter", "fonts/inter.woff", &bytes, recorded)]);
            let dist = tmp.path().join("dist");
            let router = try_build_router_with_static(
                Vec::new(),
                &compression_enabled_config(),
                test_state(),
                Some(&dist),
            )
            .expect("router builds");
            let response = router
                .oneshot(
                    Request::builder()
                        .uri("/inter")
                        .header("accept-encoding", "gzip")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK, "{label}");
            assert_eq!(
                response
                    .headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some("font/woff"),
                "{label}: woff asset must keep its font/woff MIME type"
            );
            assert_eq!(
                response.headers().get(http::header::CONTENT_ENCODING),
                None,
                "{label}: pre-compressed woff font must not be re-compressed"
            );
        }
    }

    /// The carve-out is deliberately narrow: raw SFNT fonts (`.ttf`/`.otf`) are
    /// *not* pre-compressed and must keep being gzipped. Nothing else in the
    /// repo defends this, so "tidying" the exclusion list into a `font/` prefix
    /// match would silently stop compressing them — this is the test that says
    /// no.
    #[tokio::test]
    async fn ssg_raw_sfnt_fonts_stay_compressible() {
        // Highly compressible padding, well past the size floor.
        let mut bytes = vec![0u8, 1, 0, 0];
        bytes.extend(std::iter::repeat_n(b'A', 4096));

        for (route, file, expected) in [
            ("/inter-ttf", "fonts/inter.ttf", "font/ttf"),
            ("/inter-otf", "fonts/inter.otf", "font/otf"),
        ] {
            let tmp = create_ssg_dist(&[(route, file, &bytes)]);
            let dist = tmp.path().join("dist");
            let router = try_build_router_with_static(
                Vec::new(),
                &compression_enabled_config(),
                test_state(),
                Some(&dist),
            )
            .expect("router builds");
            let response = router
                .oneshot(
                    Request::builder()
                        .uri(route)
                        .header("accept-encoding", "gzip")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK, "{route}");
            assert_eq!(
                response
                    .headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some(expected),
                "{route} must keep its raw-font MIME type"
            );
            assert_eq!(
                response
                    .headers()
                    .get(http::header::CONTENT_ENCODING)
                    .and_then(|v| v.to_str().ok()),
                Some("gzip"),
                "{route}: raw SFNT fonts are uncompressed data and must still be \
                 gzipped — only WOFF/WOFF2 embed their own compression"
            );
        }
    }

    /// A manifest-backed asset served from a nested path with a multi-dot file
    /// name (`assets/js/app.min.js`) resolves to the JavaScript MIME type. The
    /// middleware derives the type from the file name alone, so neither the
    /// intermediate directory components nor the extra `.min.` dot cause a
    /// misparse.
    #[tokio::test]
    async fn ssg_nested_multidot_asset_resolves_js_mime() {
        let js = format!("console.log({:?});", "x".repeat(256));
        let tmp = create_ssg_dist(&[("/app.js", "assets/js/app.min.js", js.as_bytes())]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/app.js")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/javascript; charset=utf-8"),
            "nested multi-dot JS asset must resolve to the JavaScript MIME type"
        );
        // JS is a compressible content type, so the layer must still gzip it.
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "compressible JS asset must be gzip-compressed"
        );
    }

    /// An extensionless generated page route (`/about` ->
    /// `about/index.html`, the shape `static_gen::url_to_file_path` produces)
    /// keeps its `text/html; charset=utf-8` type and is gzip-compressed. This
    /// pins the fallback: routes without a file extension must NOT regress to
    /// octet-stream just because the served file is `index.html`.
    #[tokio::test]
    async fn ssg_generated_html_page_keeps_text_html_and_is_compressed() {
        let html = format!("<html><body>{}</body></html>", "About us. ".repeat(128));
        let tmp = create_ssg_dist(&[("/about", "about/index.html", html.as_bytes())]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/about")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8"),
            "extensionless generated page must stay text/html, not octet-stream"
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "generated HTML page must be gzip-compressed"
        );
    }

    /// A generated `.txt` route (`/robots.txt` -> `robots.txt/index.html`) is
    /// served as `text/plain; charset=utf-8` — derived from the request
    /// route's extension, not the on-disk `index.html` file name — and is
    /// gzip-compressed. Reading the MIME off the served file would mislabel it
    /// as text/html.
    #[tokio::test]
    async fn ssg_generated_txt_route_is_text_plain_and_compressed() {
        let body_text = format!("User-agent: *\nDisallow:\n{}", "# note\n".repeat(128));
        let tmp =
            create_ssg_dist(&[("/robots.txt", "robots.txt/index.html", body_text.as_bytes())]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/robots.txt")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; charset=utf-8"),
            "generated .txt route must be text/plain, derived from the route extension"
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "compressible text/plain route must be gzip-compressed"
        );
    }

    /// A generated `.xml` route (`/sitemap.xml` -> `sitemap.xml/index.html`) is
    /// served as `application/xml`, derived from the request route's extension
    /// rather than the served `index.html` file name.
    #[tokio::test]
    async fn ssg_generated_xml_route_is_xml_mime() {
        let xml = format!(
            "<?xml version=\"1.0\"?><urlset>{}</urlset>",
            "<url><loc>https://example.com/</loc></url>".repeat(64)
        );
        let tmp = create_ssg_dist(&[("/sitemap.xml", "sitemap.xml/index.html", xml.as_bytes())]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/sitemap.xml")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/xml"),
            "generated .xml route must be application/xml, derived from the route extension"
        );
    }

    /// A generated HTML page whose slug merely *contains* a dot but ends in an
    /// UNRECOGNIZED extension (`/posts/release.v1` -> `release.v1/index.html`)
    /// stays `text/html; charset=utf-8` and is gzip-compressed. The `.v1`
    /// pseudo-extension is not an asset type, so the MIME must come from the
    /// served `index.html` — not be mislabeled octet-stream by a loose
    /// `contains('.')` heuristic.
    #[tokio::test]
    async fn ssg_dotted_slug_generated_page_stays_html_and_compressed() {
        let html = format!(
            "<html><body>{}</body></html>",
            "Release notes. ".repeat(128)
        );
        let tmp = create_ssg_dist(&[(
            "/posts/release.v1",
            "release.v1/index.html",
            html.as_bytes(),
        )]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/posts/release.v1")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8"),
            "dotted-slug generated page must stay text/html, not octet-stream"
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "dotted-slug generated HTML page must be gzip-compressed"
        );
    }

    /// A generated HTML page whose slug contains an email-like dotted suffix
    /// (`/users/alice@example.com` -> `alice@example.com/index.html`) stays
    /// `text/html; charset=utf-8`. Neither `.com` nor the `@` makes it an
    /// asset, so the MIME comes from the served `index.html`.
    #[tokio::test]
    async fn ssg_email_slug_generated_page_stays_html() {
        let html = format!("<html><body>{}</body></html>", "Profile. ".repeat(64));
        let tmp = create_ssg_dist(&[(
            "/users/alice@example.com",
            "alice@example.com/index.html",
            html.as_bytes(),
        )]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/users/alice@example.com")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8"),
            "email-like dotted-slug generated page must stay text/html"
        );
    }

    // ── #1832: the manifest's recorded Content-Type is authoritative ────────
    //
    // The six tests above pin the *derivation* that runs when a manifest
    // records nothing — a `dist/` built before #1832, or a hand-written one.
    // The tests below cover the recorded path, which removes the guess.

    /// A recorded `Content-Type` is served verbatim, even when the route
    /// extension would have produced something else. `/feed.xml` would derive
    /// `application/xml`; the manifest says `application/rss+xml`, and the
    /// manifest wins.
    #[tokio::test]
    async fn ssg_recorded_content_type_overrides_route_extension() {
        let rss = format!(
            "<?xml version=\"1.0\"?><rss>{}</rss>",
            "<item><title>Post</title></item>".repeat(64)
        );
        let tmp = create_ssg_dist_with_types(&[(
            "/feed.xml",
            "feed.xml/index.html",
            rss.as_bytes(),
            Some("application/rss+xml"),
        )]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/feed.xml")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/rss+xml"),
            "the manifest's recorded Content-Type must win over route-extension derivation"
        );
    }

    /// A type no extension in the asset table maps to (`text/calendar`) is
    /// served from an extensionless route — impossible under the old
    /// serve-time heuristic, which could only ever have said `text/html` here.
    #[tokio::test]
    async fn ssg_recorded_content_type_serves_type_outside_the_asset_table() {
        let ics = format!(
            "BEGIN:VCALENDAR\r\n{}END:VCALENDAR\r\n",
            "BEGIN:VEVENT\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n".repeat(64)
        );
        let tmp = create_ssg_dist_with_types(&[(
            "/calendar",
            "calendar/index.html",
            ics.as_bytes(),
            Some("text/calendar; charset=utf-8"),
        )]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/calendar")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/calendar; charset=utf-8"),
            "a recorded type outside the asset table must still be served"
        );
    }

    /// A recorded *binary* type on an extensionless route is honoured, and the
    /// compression layer skips it — the recorded value drives transport
    /// decisions exactly as a derived one does.
    #[tokio::test]
    async fn ssg_recorded_binary_content_type_is_not_compressed() {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend((0u32..1024).map(|i| i.wrapping_mul(2_654_435_761).to_le_bytes()[0]));
        let tmp = create_ssg_dist_with_types(&[(
            "/badge",
            "badge/index.html",
            &bytes,
            Some("image/png"),
        )]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/badge")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("image/png"),
            "recorded binary type must be honoured on an extensionless route"
        );
        assert_eq!(
            response.headers().get(http::header::CONTENT_ENCODING),
            None,
            "a recorded binary type must suppress compression just like a derived one"
        );
    }

    /// A manifest whose recorded value cannot be a header (CRLF injection
    /// attempt from a hand-edited or tampered `dist/`) must not panic the
    /// request path and must not emit the injected header: the response falls
    /// back to the derived type.
    #[tokio::test]
    async fn ssg_header_illegal_recorded_content_type_falls_back_without_panicking() {
        let html = format!("<html><body>{}</body></html>", "About us. ".repeat(128));
        let tmp = create_ssg_dist_with_types(&[(
            "/about",
            "about/index.html",
            html.as_bytes(),
            Some("text/html\r\nX-Injected: yes"),
        )]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/about")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8"),
            "a header-illegal recorded value must fall back to the derived type"
        );
        assert!(
            response.headers().get("x-injected").is_none(),
            "a CRLF in the manifest must never become a response header"
        );
    }

    /// M1 — issue evidence item 1 (fonts), on the *recorded* path. The route
    /// is extensionless and the file is `index.html`, so both legacy clues say
    /// `text/html` (compressible); only the recorded `font/woff2` stops the
    /// compression layer from re-encoding an already-compressed font.
    #[tokio::test]
    async fn ssg_recorded_woff2_type_suppresses_compression() {
        let mut bytes = b"wOF2".to_vec();
        bytes.extend((0u32..1024).map(|i| i.wrapping_mul(2_654_435_761).to_le_bytes()[0]));
        let tmp = create_ssg_dist_with_types(&[(
            "/inter",
            "inter/index.html",
            &bytes,
            Some("font/woff2"),
        )]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/inter")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("font/woff2"),
        );
        assert_eq!(
            response.headers().get(http::header::CONTENT_ENCODING),
            None,
            "a recorded pre-compressed font type must suppress compression that              the derived text/html would have allowed"
        );
    }

    /// M2 — the other direction: the recorded type *enables* compression that
    /// the derivation would have refused. `/data` → `data.bin` derives
    /// `application/octet-stream` (never compressed); the manifest says the
    /// bytes are text, and they are compressed accordingly.
    #[tokio::test]
    async fn ssg_recorded_text_type_enables_compression_derivation_would_refuse() {
        let text = "log line ".repeat(256);
        let tmp = create_ssg_dist_with_types(&[(
            "/data",
            "data.bin",
            text.as_bytes(),
            Some("text/plain; charset=utf-8"),
        )]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/data")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; charset=utf-8"),
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "the recorded text type must enable compression the octet-stream              derivation would have refused"
        );
    }

    /// M3 — a manifest route whose file is missing on disk falls through to the
    /// dynamic router. The recorded type must not leak onto that response: the
    /// header belongs to a cache *hit*, not to a manifest entry.
    #[tokio::test]
    async fn ssg_manifest_route_with_missing_file_falls_through_to_dynamic_router() {
        async fn dynamic() -> impl axum::response::IntoResponse {
            (
                [(http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                "<h1>dynamic</h1>".to_owned(),
            )
        }
        let route = Route {
            method: http::Method::GET,
            path: "/feed",
            handler: axum::routing::get(dynamic),
            name: "feed",
            api_doc: crate::openapi::ApiDoc {
                method: "GET",
                path: "/feed",
                operation_id: "feed",
                success_status: 200,
                ..Default::default()
            },
            api_version: None,
            sunset_opt_out: false,
            repository: None,
            idempotency: crate::route::RouteIdempotency::default(),
            timeout: crate::route::RouteTimeout::default(),
            seo: crate::seo::SeoRouteDefaults::EMPTY,
        };

        let tmp = create_ssg_dist_with_types(&[(
            "/feed",
            "feed/index.html",
            b"<rss/>",
            Some("application/rss+xml"),
        )]);
        let dist = tmp.path().join("dist");
        // The manifest still lists the route; the generated file is gone.
        std::fs::remove_file(dist.join("feed/index.html")).expect("remove generated file");

        let router = try_build_router_with_static(
            vec![route],
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
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
            Some("text/html; charset=utf-8"),
            "a manifest miss on disk must serve the dynamic handler's own type,              not the recorded one"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"<h1>dynamic</h1>");
    }

    /// M7 — a trailing-slash request normalizes to the same manifest entry and
    /// therefore carries the same recorded type. This is a case the old
    /// heuristic got wrong: `/robots.txt/` has no recognized extension on its
    /// final segment, so route-extension derivation fell through to
    /// `index.html` and said `text/html`.
    #[tokio::test]
    async fn ssg_trailing_slash_request_serves_recorded_content_type() {
        let body_text = format!("User-agent: *\nDisallow:\n{}", "# note\n".repeat(64));
        let tmp = create_ssg_dist_with_types(&[(
            "/robots.txt",
            "robots.txt/index.html",
            body_text.as_bytes(),
            Some("text/plain; charset=utf-8"),
        )]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/robots.txt/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; charset=utf-8"),
            "the normalized path drives both the manifest lookup and the header"
        );
    }

    /// A `HEAD` request on a recorded route carries the same recorded type as
    /// the `GET`, with no body.
    #[tokio::test]
    async fn ssg_head_request_carries_recorded_content_type() {
        let tmp = create_ssg_dist_with_types(&[(
            "/feed",
            "feed/index.html",
            b"<rss/>",
            Some("application/rss+xml"),
        )]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            Vec::new(),
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .method(http::Method::HEAD)
                    .uri("/feed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/rss+xml")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty(), "HEAD response must have an empty body");
    }

    /// End-to-end: a real `render_static_routes` build feeds the real serve
    /// path. The three routes that each needed a serve-time heuristic
    /// correction during #1819 — a generated `.txt`, a generated `.xml`, and a
    /// dotted-slug HTML page — plus two the heuristic could never get right:
    /// `/feed`, extensionless but RSS, and `/notes.txt`, whose handler declares
    /// JSON in direct contradiction of its slug.
    ///
    /// It asserts the recorded manifest values *and* the served headers.
    /// Asserting only the headers would be weak: for the three #1819 routes the
    /// old heuristic produced the same answer, so the header alone proves
    /// nothing about recording. The manifest assertions are what the pre-#1832
    /// code cannot satisfy at all, and `/notes.txt` is what no derivation from
    /// the route or the file name could ever produce.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // five routes x (handler + build + serve assertions)
    async fn ssg_generated_manifest_round_trips_content_types_end_to_end() {
        async fn robots() -> impl axum::response::IntoResponse {
            (
                [(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                format!("User-agent: *\nDisallow:\n{}", "# note\n".repeat(128)),
            )
        }
        // Bodies are padded past the compression size floor throughout, so the
        // `Content-Encoding` assertions below turn on the recorded *type* and
        // never on the body being too small to bother with.
        async fn sitemap() -> impl axum::response::IntoResponse {
            (
                [(http::header::CONTENT_TYPE, "application/xml")],
                format!(
                    "<?xml version=\"1.0\"?><urlset>{}</urlset>",
                    "<url><loc>https://example.com/</loc></url>".repeat(16)
                ),
            )
        }
        async fn release_notes() -> impl axum::response::IntoResponse {
            axum::response::Html(format!(
                "<html><body>{}</body></html>",
                "Release notes. ".repeat(128)
            ))
        }
        async fn feed() -> impl axum::response::IntoResponse {
            (
                [(http::header::CONTENT_TYPE, "application/rss+xml")],
                format!("<rss><channel>{}</channel></rss>", "<item/>".repeat(64)),
            )
        }
        // Slug says `.txt`, handler says JSON. No derivation from the route or
        // the served file name can produce this; only recording can.
        async fn notes() -> impl axum::response::IntoResponse {
            (
                [(http::header::CONTENT_TYPE, "application/json")],
                format!(
                    r#"{{"notes":[{}]}}"#,
                    r#""note","#.repeat(32).trim_end_matches(',')
                ),
            )
        }
        // The third #1819 case, on the recorded path: a slug whose final
        // segment contains dots and an `@` but which is plain HTML.
        async fn profile() -> impl axum::response::IntoResponse {
            axum::response::Html(format!(
                "<html><body>{}</body></html>",
                "Alice. ".repeat(128)
            ))
        }
        // The behaviour change the changelog calls out as breaking, proven at
        // the served-header level and not just in the manifest: an
        // *extensionless* route returning a bare `String` declares
        // `text/plain; charset=utf-8`, and that is now what is served — where
        // the pre-#1832 heuristic assumed `text/html` from `about/index.html`.
        async fn about() -> impl axum::response::IntoResponse {
            "About us. ".repeat(128)
        }

        fn meta(path: &'static str, name: &'static str) -> crate::static_gen::StaticRouteMeta {
            crate::static_gen::StaticRouteMeta {
                path,
                name,
                revalidate: None,
                params_fn: None,
                seo: crate::seo::SeoRouteDefaults::EMPTY,
            }
        }

        let build_router = axum::Router::new()
            .route("/robots.txt", axum::routing::get(robots))
            .route("/sitemap.xml", axum::routing::get(sitemap))
            .route("/posts/release.v1", axum::routing::get(release_notes))
            .route("/feed", axum::routing::get(feed))
            .route("/notes.txt", axum::routing::get(notes))
            .route("/users/alice@example.com", axum::routing::get(profile))
            .route("/about", axum::routing::get(about));

        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        crate::static_gen::render_static_routes(
            build_router,
            &[
                meta("/robots.txt", "robots"),
                meta("/sitemap.xml", "sitemap"),
                meta("/posts/release.v1", "release_notes"),
                meta("/feed", "feed"),
                meta("/notes.txt", "notes"),
                meta("/users/alice@example.com", "profile"),
                meta("/about", "about"),
            ],
            &dist,
        )
        .await
        .expect("static build succeeds");

        // Every generated page is stored as `<route>/index.html` — the layout
        // that made the serve-time file-name heuristic unreliable.
        for file in [
            "robots.txt/index.html",
            "sitemap.xml/index.html",
            "posts/release.v1/index.html",
            "feed/index.html",
            "notes.txt/index.html",
            "users/alice@example.com/index.html",
            "about/index.html",
        ] {
            assert!(dist.join(file).is_file(), "{file} must have been generated");
        }

        // Route, recorded/served type, and whether the type makes the body
        // compressible — the recorded type's whole transport consequence.
        let expected = [
            ("/robots.txt", "text/plain; charset=utf-8", true),
            ("/sitemap.xml", "application/xml", true),
            ("/posts/release.v1", "text/html; charset=utf-8", true),
            ("/feed", "application/rss+xml", true),
            ("/notes.txt", "application/json", true),
            ("/users/alice@example.com", "text/html; charset=utf-8", true),
            ("/about", "text/plain; charset=utf-8", true),
        ];

        // The build recorded the intended type for every route. This is the
        // assertion the pre-#1832 generator cannot satisfy — it wrote no type
        // at all — and it is what makes the header assertions below meaningful
        // rather than a restatement of the old heuristic.
        let manifest =
            crate::static_gen::StaticManifest::load(&dist.join("manifest.json")).expect("manifest");
        for (route, content_type, _) in expected {
            assert_eq!(
                manifest.routes[route].content_type.as_deref(),
                Some(content_type),
                "{route} must carry its declared type in the manifest"
            );
        }

        for (route, content_type, compressible) in expected {
            let router = try_build_router_with_static(
                Vec::new(),
                &compression_enabled_config(),
                test_state(),
                Some(&dist),
            )
            .expect("router builds");
            let response = router
                .oneshot(
                    Request::builder()
                        .uri(route)
                        .header("accept-encoding", "gzip")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK, "{route}");
            assert_eq!(
                response
                    .headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some(content_type),
                "{route} must be served as the type its handler declared at build time"
            );
            assert_eq!(
                response
                    .headers()
                    .get(http::header::CONTENT_ENCODING)
                    .and_then(|v| v.to_str().ok())
                    == Some("gzip"),
                compressible,
                "{route}: the recorded type must drive compression negotiation"
            );
        }
    }

    /// A dynamic fallback route (not in the manifest) is compressed the same way
    /// as SSG pages, confirming parity between static-first and dynamic
    /// responses.
    #[tokio::test]
    async fn ssg_dynamic_fallback_route_is_gzip_compressed() {
        async fn dynamic() -> impl axum::response::IntoResponse {
            (
                [(http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                format!(
                    "<html><body>{}</body></html>",
                    "dynamic content ".repeat(64)
                ),
            )
        }
        let route = Route {
            method: http::Method::GET,
            path: "/dynamic",
            handler: axum::routing::get(dynamic),
            name: "dynamic",
            api_doc: crate::openapi::ApiDoc {
                method: "GET",
                path: "/dynamic",
                operation_id: "dynamic",
                success_status: 200,
                ..Default::default()
            },
            api_version: None,
            sunset_opt_out: false,
            repository: None,
            idempotency: crate::route::RouteIdempotency::default(),
            timeout: crate::route::RouteTimeout::default(),
            seo: crate::seo::SeoRouteDefaults::EMPTY,
        };

        // Manifest does NOT contain /dynamic, so the request falls through to
        // the dynamic router.
        let tmp = create_ssg_dist(&[("/", "index.html", b"<h1>home</h1>")]);
        let dist = tmp.path().join("dist");

        let router = try_build_router_with_static(
            vec![route],
            &compression_enabled_config(),
            test_state(),
            Some(&dist),
        )
        .expect("router builds");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/dynamic")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "dynamic fallback route must be compressed just like SSG pages"
        );
    }

    fn create_static_dist(revalidate: Option<u64>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let dist = dir.path().join("dist");
        std::fs::create_dir_all(dist.join("about")).expect("mkdir about");
        std::fs::write(dist.join("index.html"), b"<h1>Home</h1>").expect("write index");
        std::fs::write(dist.join("about/index.html"), b"<h1>About</h1>").expect("write about");

        let mut routes = std::collections::HashMap::new();
        routes.insert(
            "/".to_owned(),
            crate::static_gen::ManifestEntry::new("index.html".to_owned()),
        );
        routes.insert(
            "/about".to_owned(),
            crate::static_gen::ManifestEntry::new("about/index.html".to_owned())
                .with_revalidate(revalidate),
        );

        let manifest = crate::static_gen::StaticManifest {
            generated_at: "2026-05-18T00:00:00Z".to_owned(),
            autumn_version: "0.5.0".to_owned(),
            routes,
        };
        let json = serde_json::to_string(&manifest).expect("serialize manifest");
        std::fs::write(dist.join("manifest.json"), json).expect("write manifest");
        dir
    }

    #[tokio::test]
    async fn static_serving_serves_get_request_inside_user_layers() {
        let tmp = create_static_dist(None);
        let dist = tmp.path().join("dist");
        let config = AutumnConfig::default();

        let router = try_build_router_with_static(Vec::new(), &config, test_state(), Some(&dist))
            .expect("router builds");

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/about")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"<h1>About</h1>");
    }

    #[tokio::test]
    async fn static_serving_serves_head_request() {
        let tmp = create_static_dist(None);
        let dist = tmp.path().join("dist");
        let config = AutumnConfig::default();

        let router = try_build_router_with_static(Vec::new(), &config, test_state(), Some(&dist))
            .expect("router builds");

        let response = router
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri("/about")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty(), "HEAD response body should be empty");
    }

    #[tokio::test]
    async fn static_serving_normalizes_trailing_slash() {
        let tmp = create_static_dist(None);
        let dist = tmp.path().join("dist");
        let config = AutumnConfig::default();

        let router = try_build_router_with_static(Vec::new(), &config, test_state(), Some(&dist))
            .expect("router builds");

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/about/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn static_serving_falls_through_for_unknown_route() {
        let tmp = create_static_dist(None);
        let dist = tmp.path().join("dist");
        let config = AutumnConfig::default();

        let router = try_build_router_with_static(Vec::new(), &config, test_state(), Some(&dist))
            .expect("router builds");

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/not-in-manifest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn static_serving_skipped_when_no_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).expect("mkdir dist");
        let config = AutumnConfig::default();

        let router = try_build_router_with_static(Vec::new(), &config, test_state(), Some(&dist))
            .expect("router builds even without manifest");

        let response = router
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn static_serving_with_isr_manifest_builds_successfully() {
        let tmp = create_static_dist(Some(3600));
        let dist = tmp.path().join("dist");
        let config = AutumnConfig::default();

        let router = try_build_router_with_static(Vec::new(), &config, test_state(), Some(&dist))
            .expect("router with ISR manifest should build");

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/about")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[cfg(test)]
mod trusted_host_tests {
    use super::*;
    use axum::body::Body;
    use http::Request;
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn trusted_host_allows_matching_and_blocks_nonmatching() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com".into(), ".example.com".into()];
        let state = crate::state::AppState::for_test();
        let router = build_router(vec![], &cfg, state);

        let ok = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .header("host", "api.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::NOT_FOUND);

        let blocked = router
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .header("host", "evil.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn trusted_host_wildcard_allows_any_host() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["*".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .header("host", "anything.example")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn trusted_host_bypasses_probe_paths() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/actuator/health")
                    .header("host", "evil.com")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn trusted_host_bypasses_actuator_health_path() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/actuator/health")
                    .header("host", "evil.com")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// `probe_bypass_paths()` is meant to be the single canonical definition
    /// of "which exact paths bypass admission-style gates" — `TrustedHostPolicy`
    /// and `StartupBarrierState` must derive their own bypass sets from it
    /// rather than each re-implementing the same list, so a change to
    /// `probe_bypass_paths()` (or `config.health.*`) is automatically
    /// reflected in both without touching either of them directly.
    #[test]
    fn probe_bypass_paths_is_the_single_source_for_trusted_host_and_startup_barrier() {
        let mut cfg = AutumnConfig::default();
        cfg.health.path = "/custom-health-check".into();
        let expected = probe_bypass_paths(&cfg);
        assert!(expected.contains(&"/custom-health-check".to_string()));

        let trusted_host = TrustedHostPolicy::from_config(&cfg);
        for path in &expected {
            assert!(
                trusted_host.probe_bypass_paths.contains(path),
                "TrustedHostPolicy must derive its bypass set from probe_bypass_paths(): missing {path}"
            );
        }

        let state = crate::state::AppState::for_test();
        let barrier = StartupBarrierState::from_config(&cfg, &state);
        for path in &expected {
            assert!(
                barrier.allows_path(path),
                "StartupBarrierState must derive its bypass set from probe_bypass_paths(): missing {path}"
            );
        }
    }

    /// Regression guard (#1627): the `StartupBarrierState` allow-list, which is
    /// seeded from `actuator_endpoint_paths`, must permit the mutating
    /// `POST {prefix}/webhooks/replay` route to bypass the startup barrier. A
    /// prior fix removed the path from `actuator_endpoint_paths` to kill a
    /// phantom GET in the route listing, which also silently dropped it from
    /// this bypass set.
    #[cfg(feature = "http-client")]
    #[test]
    fn startup_barrier_allows_webhook_replay_post_path() {
        let mut cfg = AutumnConfig::default();
        cfg.actuator.sensitive = true;
        let state = crate::state::AppState::for_test();
        let barrier = StartupBarrierState::from_config(&cfg, &state);
        let replay_path =
            crate::actuator::actuator_route_path(&cfg.actuator.prefix, "/webhooks/replay");
        assert!(
            barrier.allows_path(&replay_path),
            "startup barrier must allow {replay_path} to bypass admission"
        );
    }

    /// `/ready` must keep answering 200 while maintenance mode is ON (issue
    /// #1621, T1.25) — this pins an EXISTING, deliberate contract that a fleet
    /// deploy depends on, so nobody "fixes" it later.
    ///
    /// [`build_maintenance_layer`] wires `with_probe_paths(probe_bypass_paths(cfg))`,
    /// which includes `health.ready_path`. The consequence operators must be told
    /// about: **maintenance mode does not drain a host from a load balancer.** The
    /// host stays in rotation (its `/ready` is green) and serves 503s with
    /// `Retry-After` to real users — by design, because the alternative would make
    /// every LB-fronted deployment eject every host the moment maintenance is
    /// enabled. `autumn deploy status` therefore reports readiness and maintenance
    /// as SEPARATE columns, and `autumn deploy maintenance on` says so out loud.
    #[tokio::test]
    async fn ready_probe_stays_green_while_maintenance_mode_is_active() {
        let cfg = AutumnConfig::default();
        let state = crate::state::AppState::for_test();
        // Startup must be complete or `/ready` is 503 for an unrelated reason and
        // the assertion below would pass vacuously.
        state.probes().mark_startup_complete();
        let maintenance = crate::maintenance::MaintenanceState::new();
        maintenance.enable(crate::maintenance::MaintenanceConfig::default());
        state.insert_extension(maintenance);
        let router = build_router(vec![], &cfg, state);

        let ready = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&cfg.health.ready_path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            ready.status(),
            StatusCode::OK,
            "/ready must BYPASS maintenance mode: it is the load balancer's health \
             signal, and gating it would eject every host from rotation the moment \
             maintenance is enabled (#1621)"
        );

        // …while ordinary traffic IS gated, proving the layer is actually active.
        let app_route = router
            .oneshot(
                Request::builder()
                    .uri("/some-app-route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            app_route.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "maintenance mode must still gate non-probe traffic"
        );
    }

    #[tokio::test]
    async fn trusted_host_release_rejects_loopback_unless_listed() {
        let mut cfg = AutumnConfig {
            profile: Some("prod".into()),
            ..AutumnConfig::default()
        };
        cfg.security.trusted_hosts.hosts = vec!["example.com".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .header("host", "localhost")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn trusted_host_uses_uri_authority_when_host_header_missing() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("http://EXAMPLE.COM/nope")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn trusted_host_accepts_bracketed_ipv6_loopback_in_dev() {
        let cfg = AutumnConfig::default();
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .header("host", "[::1]:3000")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn trusted_host_matching_is_case_insensitive() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .header("host", "EXAMPLE.COM")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn trusted_host_rejects_malformed_port() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .header("host", "example.com:abc")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn trusted_host_rejects_empty_port_suffix() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .header("host", "example.com:")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn trusted_host_rejects_bracketed_reg_name() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .header("host", "[example.com]")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    #[tokio::test]
    async fn trusted_host_configured_trailing_dot_matches_normalized_host() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com.".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .header("host", "example.com")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn trusted_host_accepts_trailing_dot_fqdn() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .header("host", "example.com.")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn trusted_host_bypasses_custom_probe_path_only() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com".into()];
        cfg.health.path = "/healthz".into();
        cfg.health.startup_path = "/startupz".into();
        cfg.health.ready_path = "/readyz".into();
        cfg.health.live_path = "/livez".into();
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());

        let bypassed = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("host", "evil.com")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(bypassed.status(), StatusCode::OK);

        let not_bypassed = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("host", "evil.com")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(not_bypassed.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn trusted_host_does_not_bypass_non_get_probe_path_requests() {
        let mut cfg = AutumnConfig::default();
        cfg.security.trusted_hosts.hosts = vec!["example.com".into()];
        let router = build_router(vec![], &cfg, crate::state::AppState::for_test());
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/health")
                    .header("host", "evil.com")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // ── Global body-size limit (AC: DefaultBodyLimit covers all content types) ──

    #[tokio::test]
    async fn apply_upload_middleware_rejects_oversized_json_body() {
        let mut config = AutumnConfig::default();
        config.security.upload.max_request_size_bytes = 100; // 100-byte limit

        let base: axum::Router<AppState> = axum::Router::new().route(
            "/data",
            axum::routing::post(|_: axum::body::Bytes| async { "ok" }),
        );
        let router =
            apply_upload_middleware(base, &config).with_state(crate::state::AppState::for_test());

        // 200 bytes of JSON-shaped content exceeds the 100-byte cap.
        let big_body = "x".repeat(200);
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/data")
                    .header("content-type", "application/json")
                    .body(Body::from(big_body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "oversized body must be rejected with 413 regardless of content type"
        );
    }

    #[tokio::test]
    async fn apply_upload_middleware_accepts_body_within_limit() {
        let mut config = AutumnConfig::default();
        config.security.upload.max_request_size_bytes = 1024;

        let base: axum::Router<AppState> = axum::Router::new().route(
            "/data",
            axum::routing::post(|_: axum::body::Bytes| async { "ok" }),
        );
        let router =
            apply_upload_middleware(base, &config).with_state(crate::state::AppState::for_test());

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/data")
                    .header("content-type", "application/json")
                    .body(Body::from("hello"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    // ── Per-request timeout (AC: 503 on timeout, metrics, WARN log) ──────────

    /// Empty per-route override table (no route-level overrides).
    fn no_route_timeouts() -> RouteTimeoutTable {
        std::sync::Arc::new(std::collections::HashMap::new())
    }

    /// Build a single-entry override table for `GET <path>` (the method the
    /// unit-test routers below register).
    fn get_route_timeouts(path: &str, timeout: crate::route::RouteTimeout) -> RouteTimeoutTable {
        let mut by_method = std::collections::HashMap::new();
        by_method.insert(http::Method::GET, timeout);
        let mut table = std::collections::HashMap::new();
        table.insert(path.to_owned(), by_method);
        std::sync::Arc::new(table)
    }

    #[tokio::test(start_paused = true)]
    async fn request_timeout_returns_503_when_exceeded() {
        let mut config = AutumnConfig::default();
        config.server.timeouts.request_timeout_ms = Some(100);

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                // This sleep is much longer than the 100ms timeout.
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                "ok"
            }),
        );

        // Place timeout inner to RequestIdLayer (matches apply_middleware ordering).
        let router = apply_request_timeout_middleware(
            router,
            &config,
            state.metrics.clone(),
            no_route_timeouts(),
            false,
        )
        .layer(RequestIdLayer::default())
        .with_state(state);

        let response = router
            .oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a slow handler must trigger 503"
        );
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json"),
            "timeout response must use Problem Details content type"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn request_timeout_increments_metric() {
        let mut config = AutumnConfig::default();
        config.server.timeouts.request_timeout_ms = Some(100);

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                "ok"
            }),
        );

        let router = apply_request_timeout_middleware(
            router,
            &config,
            state.metrics.clone(),
            no_route_timeouts(),
            false,
        )
        .layer(RequestIdLayer::default())
        .with_state(state.clone());

        router
            .oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let snap = state.metrics.snapshot();
        assert_eq!(
            snap.http.request_timeouts_total, 1,
            "autumn_request_timeouts_total must be incremented on timeout"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn render_deadline_exempt_marker_skips_timeout() {
        let mut config = AutumnConfig::default();
        config.server.timeouts.request_timeout_ms = Some(100);

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                // Far longer than the 100ms deadline; the paused clock advances
                // automatically once the task is otherwise idle.
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                "ok"
            }),
        );

        let router = apply_request_timeout_middleware(
            router,
            &config,
            state.metrics.clone(),
            no_route_timeouts(),
            false,
        )
        .layer(RequestIdLayer::default())
        .with_state(state);

        // A live inbound request (no marker) is bounded by the deadline -> 503.
        let live = router
            .clone()
            .oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            live.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a live request to a slow handler must still time out"
        );

        // An internal build/ISR render carrying `RenderDeadlineExempt` is exempt
        // and runs to completion.
        let exempt = router
            .oneshot(
                Request::builder()
                    .uri("/slow")
                    .extension(crate::static_gen::RenderDeadlineExempt)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            exempt.status(),
            StatusCode::OK,
            "the build/ISR render marker must exempt the request from the deadline"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn request_timeout_503_mirrors_cors_headers() {
        let mut config = AutumnConfig::default();
        config.server.timeouts.request_timeout_ms = Some(100);
        // CORS is configured with a concrete allowlist (the reflected-origin path).
        config.cors.allowed_origins = vec!["https://app.example.com".to_owned()];
        config.cors.allow_credentials = true;

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                "ok"
            }),
        );

        // `mirror_cors = true`, matching the main ingress stack where the timeout
        // layer is outside `CorsLayer` and the 503 would otherwise be opaque.
        let router = apply_request_timeout_middleware(
            router,
            &config,
            state.metrics.clone(),
            no_route_timeouts(),
            true,
        )
        .layer(RequestIdLayer::default())
        .with_state(state);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/slow")
                    .header("origin", "https://app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("https://app.example.com"),
            "an allowed origin must be reflected on the timeout 503 so browsers can read it"
        );
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-credentials")
                .and_then(|v| v.to_str().ok()),
            Some("true"),
            "credentials flag must be mirrored when configured"
        );
        assert!(
            response
                .headers()
                .get_all("vary")
                .iter()
                .any(|v| v.to_str().is_ok_and(|s| s.eq_ignore_ascii_case("origin"))),
            "a reflected origin must carry Vary: origin"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn request_timeout_503_omits_cors_for_disallowed_origin() {
        let mut config = AutumnConfig::default();
        config.server.timeouts.request_timeout_ms = Some(100);
        config.cors.allowed_origins = vec!["https://app.example.com".to_owned()];

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                "ok"
            }),
        );

        let router = apply_request_timeout_middleware(
            router,
            &config,
            state.metrics.clone(),
            no_route_timeouts(),
            true,
        )
        .layer(RequestIdLayer::default())
        .with_state(state);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/slow")
                    .header("origin", "https://evil.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none(),
            "a disallowed origin must not be reflected, mirroring CorsLayer"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn request_timeout_response_includes_request_id_header() {
        let mut config = AutumnConfig::default();
        config.server.timeouts.request_timeout_ms = Some(100);

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                "ok"
            }),
        );

        let router = apply_request_timeout_middleware(
            router,
            &config,
            state.metrics.clone(),
            no_route_timeouts(),
            false,
        )
        .layer(RequestIdLayer::default())
        .with_state(state);

        let response = router
            .oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        // X-Request-Id is added by RequestIdLayer on the egress path.
        assert!(
            response.headers().contains_key("x-request-id"),
            "503 response must carry the X-Request-Id header"
        );

        // The body must be a well-formed Problem Details document.
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["status"], 503);
    }

    #[tokio::test]
    async fn request_timeout_disabled_when_none() {
        let config = AutumnConfig::default(); // request_timeout_ms = None

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> =
            axum::Router::new().route("/fast", axum::routing::get(|| async { "pong" }));

        let router = apply_request_timeout_middleware(
            router,
            &config,
            state.metrics.clone(),
            no_route_timeouts(),
            false,
        )
        .with_state(state);

        let response = router
            .oneshot(Request::builder().uri("/fast").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn request_timeout_zero_treated_as_disabled() {
        let mut config = AutumnConfig::default();
        config.server.timeouts.request_timeout_ms = Some(0); // 0 = disabled

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> =
            axum::Router::new().route("/fast", axum::routing::get(|| async { "pong" }));

        let router = apply_request_timeout_middleware(
            router,
            &config,
            state.metrics.clone(),
            no_route_timeouts(),
            false,
        )
        .with_state(state);

        let response = router
            .oneshot(Request::builder().uri("/fast").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    // Exercises the warn branch when no RequestIdLayer is present (no request_id
    // extension), keeping coverage of the `None` request-id arm.
    #[tokio::test(start_paused = true)]
    async fn request_timeout_503_without_request_id_layer() {
        let mut config = AutumnConfig::default();
        config.server.timeouts.request_timeout_ms = Some(100);

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                "ok"
            }),
        );

        // No RequestIdLayer — exercises the `request_id: None` branch in
        // `RequestTimeoutService`.
        let router = apply_request_timeout_middleware(
            router,
            &config,
            state.metrics.clone(),
            no_route_timeouts(),
            false,
        )
        .with_state(state);

        let response = router
            .oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // AC4: a per-route `Override` extends the deadline so a known-slow route
    // outlives the (smaller) global timeout.
    #[tokio::test(start_paused = true)]
    async fn request_timeout_per_route_override_extends_deadline() {
        let mut config = AutumnConfig::default();
        config.server.timeouts.request_timeout_ms = Some(100); // tight global

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> = axum::Router::new().route(
            "/export",
            axum::routing::get(|| async {
                // Longer than the 100ms global, shorter than the 10s override.
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                "report"
            }),
        );

        let table = get_route_timeouts(
            "/export",
            crate::route::RouteTimeout::Override(std::time::Duration::from_secs(10)),
        );
        let router =
            apply_request_timeout_middleware(router, &config, state.metrics.clone(), table, false)
                .with_state(state);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/export")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the override must let the slow route complete past the global deadline"
        );
    }

    // AC4: a per-route `Disabled` exempts the route from the global timeout.
    #[tokio::test(start_paused = true)]
    async fn request_timeout_per_route_disabled_exempts_route() {
        let mut config = AutumnConfig::default();
        config.server.timeouts.request_timeout_ms = Some(100);

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> = axum::Router::new().route(
            "/stream",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                "done"
            }),
        );

        let table = get_route_timeouts("/stream", crate::route::RouteTimeout::Disabled);
        let router =
            apply_request_timeout_middleware(router, &config, state.metrics.clone(), table, false)
                .with_state(state.clone());

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            state.metrics.snapshot().http.request_timeouts_total,
            0,
            "an exempt route must not record a timeout"
        );
    }

    // AC4: an `Override` enables the layer even when the global timeout is off.
    #[tokio::test(start_paused = true)]
    async fn request_timeout_override_active_when_global_disabled() {
        let config = AutumnConfig::default(); // global timeout disabled (None)

        let state = crate::state::AppState::for_test();
        let router: axum::Router<AppState> = axum::Router::new().route(
            "/export",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                "report"
            }),
        );

        let table = get_route_timeouts(
            "/export",
            crate::route::RouteTimeout::Override(std::time::Duration::from_millis(100)),
        );
        let router =
            apply_request_timeout_middleware(router, &config, state.metrics.clone(), table, false)
                .with_state(state);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/export")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a per-route override must be enforced even with the global timeout off"
        );
    }

    #[test]
    fn build_route_timeout_table_is_empty_without_routes() {
        // End-to-end keying (top-level + nested groups) is covered by the
        // `request_timeout` integration tests via the macro attribute; here we
        // assert the no-route base case yields a zero-overhead empty table.
        let table = build_route_timeout_table(&[], &[], &AutumnConfig::default());
        assert!(table.is_empty(), "no routes ⇒ empty override table");
    }

    /// Build a minimal `Route` carrying just the fields `build_route_timeout_table`
    /// reads (method, path, timeout); the handler is a no-op.
    fn timeout_route(
        method: http::Method,
        path: &'static str,
        timeout: crate::route::RouteTimeout,
    ) -> Route {
        async fn noop() -> &'static str {
            "ok"
        }
        Route {
            method,
            path,
            handler: axum::routing::get(noop),
            name: "noop",
            api_doc: crate::openapi::ApiDoc::default(),
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout,
            api_version: None,
            sunset_opt_out: false,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
        }
    }

    #[test]
    fn build_route_timeout_table_normalizes_method_aliases() {
        let override_10s = crate::route::RouteTimeout::Override(std::time::Duration::from_secs(10));
        let routes = vec![
            // A GET handler also serves HEAD in axum.
            timeout_route(http::Method::GET, "/export", override_10s),
            // A `#[ws]` route records the synthetic `WS` method but the upgrade
            // arrives as GET.
            timeout_route(
                http::Method::from_bytes(b"WS").unwrap(),
                "/live",
                crate::route::RouteTimeout::Disabled,
            ),
            // A non-aliased method keys only itself.
            timeout_route(http::Method::POST, "/submit", override_10s),
        ];

        let table = build_route_timeout_table(&routes, &[], &AutumnConfig::default());

        // GET override is reachable via both GET and HEAD.
        let export = table.get("/export").expect("/export keyed");
        assert_eq!(export.get(&http::Method::GET), Some(&override_10s));
        assert_eq!(
            export.get(&http::Method::HEAD),
            Some(&override_10s),
            "a GET override must also cover the HEAD alias axum serves"
        );

        // WS override is reachable via the GET the upgrade actually uses, and is
        // NOT left under the synthetic `WS` method the lookup never sees.
        let live = table.get("/live").expect("/live keyed");
        assert_eq!(
            live.get(&http::Method::GET),
            Some(&crate::route::RouteTimeout::Disabled),
            "a WS override must be keyed under the GET the upgrade arrives as"
        );
        assert!(
            live.get(&http::Method::from_bytes(b"WS").unwrap())
                .is_none(),
            "the synthetic WS method is never seen at lookup time"
        );

        // A non-aliased method keys only itself — no HEAD bleed.
        let submit = table.get("/submit").expect("/submit keyed");
        assert_eq!(submit.get(&http::Method::POST), Some(&override_10s));
        assert!(submit.get(&http::Method::HEAD).is_none());
    }

    #[test]
    fn build_route_timeout_table_keys_scoped_root_by_axum_matched_path() {
        // A scoped group whose prefix carries a trailing slash mounts its `/`
        // child at "/api/" in axum (verified by
        // `join_nested_path_matches_axum_matched_path`), so the override must be
        // keyed there — not at "/api" — or the runtime `MatchedPath` lookup
        // misses and the per-route timeout is silently never enforced.
        let override_5s = crate::route::RouteTimeout::Override(std::time::Duration::from_secs(5));
        let make_group = |prefix: &str| crate::app::ScopedGroup {
            prefix: prefix.to_owned(),
            routes: vec![timeout_route(http::Method::GET, "/", override_5s)],
            source: crate::route_listing::RouteSource::User,
            apply_layer: Box::new(|r| r),
        };

        let table =
            build_route_timeout_table(&[], &[make_group("/api/")], &AutumnConfig::default());
        assert_eq!(
            table.get("/api/").and_then(|m| m.get(&http::Method::GET)),
            Some(&override_5s),
            "trailing-slash scoped root must key the override at /api/"
        );
        assert!(
            table.get("/api").is_none(),
            "the stripped /api key would never match the runtime lookup"
        );

        // The no-trailing-slash form still keys at "/api".
        let table = build_route_timeout_table(&[], &[make_group("/api")], &AutumnConfig::default());
        assert_eq!(
            table.get("/api").and_then(|m| m.get(&http::Method::GET)),
            Some(&override_5s),
        );
    }

    /// Codex review (P1): `Router::nest("/{locale}", ...)` mounts each locale
    /// under a literal segment, so a locale-prefixed request's `MatchedPath`
    /// is `/{locale}{path}` verbatim — the timeout table must carry an entry
    /// there too, or a `timeout = "off"` long-poll route gets silently
    /// cancelled by the global deadline once locale-prefix routing is on.
    #[cfg(feature = "i18n")]
    #[test]
    fn build_route_timeout_table_expands_entries_under_each_locale_prefix() {
        let disabled = crate::route::RouteTimeout::Disabled;
        let routes = vec![timeout_route(http::Method::GET, "/events", disabled)];
        let mut config = AutumnConfig::default();
        config.i18n.locale_prefix_enabled = true;
        config.i18n.supported_locales = vec!["en".to_owned(), "es".to_owned()];

        let table = build_route_timeout_table(&routes, &[], &config);

        for path in ["/events", "/en/events", "/es/events"] {
            assert_eq!(
                table.get(path).and_then(|m| m.get(&http::Method::GET)),
                Some(&disabled),
                "expected a timeout override at {path}"
            );
        }
    }

    /// A route excluded from locale-prefix routing never gets nested under
    /// `/{locale}`, so its timeout table entry must stay bare-path only.
    #[cfg(feature = "i18n")]
    #[test]
    fn build_route_timeout_table_does_not_expand_excluded_routes() {
        let override_5s = crate::route::RouteTimeout::Override(std::time::Duration::from_secs(5));
        let routes = vec![timeout_route(http::Method::GET, "/api/keys", override_5s)];
        let mut config = AutumnConfig::default();
        config.i18n.locale_prefix_enabled = true;
        config.i18n.supported_locales = vec!["en".to_owned(), "es".to_owned()];
        config.i18n.locale_prefix_exclude = vec!["/api".to_owned()];

        let table = build_route_timeout_table(&routes, &[], &config);

        assert_eq!(
            table
                .get("/api/keys")
                .and_then(|m| m.get(&http::Method::GET)),
            Some(&override_5s)
        );
        assert!(
            table.get("/en/api/keys").is_none(),
            "an excluded route must not gain a locale-prefixed timeout entry"
        );
    }

    /// Locale-prefix routing off (the default) must never expand the table,
    /// regardless of what `supported_locales` happens to contain.
    #[cfg(feature = "i18n")]
    #[test]
    fn build_route_timeout_table_does_not_expand_when_locale_prefix_disabled() {
        let override_5s = crate::route::RouteTimeout::Override(std::time::Duration::from_secs(5));
        let routes = vec![timeout_route(http::Method::GET, "/events", override_5s)];
        let mut config = AutumnConfig::default();
        assert!(!config.i18n.locale_prefix_enabled);
        config.i18n.supported_locales = vec!["en".to_owned(), "es".to_owned()];

        let table = build_route_timeout_table(&routes, &[], &config);

        assert_eq!(table.len(), 1, "only the bare-path entry should exist");
        assert!(table.get("/en/events").is_none());
    }

    // ----------------------------------------------------------------------
    // static_gate: middleware that runs before the static cache lookup (#848)
    // ----------------------------------------------------------------------

    /// Build a `CustomLayerRegistration` wrapping a `from_fn` gate that
    /// redirects (302 → /login) any request lacking an `x-authed` header.
    fn redirect_gate_registration() -> crate::app::CustomLayerRegistration {
        let gate = axum::middleware::from_fn(
            |req: axum::extract::Request, next: axum::middleware::Next| async move {
                if req.headers().contains_key("x-authed") {
                    next.run(req).await
                } else {
                    http::Response::builder()
                        .status(StatusCode::FOUND)
                        .header(http::header::LOCATION, "/login")
                        .body(Body::empty())
                        .unwrap()
                }
            },
        );
        crate::app::CustomLayerRegistration {
            type_id: std::any::TypeId::of::<()>(),
            type_name: "redirect_gate",
            layer: tower::util::BoxCloneSyncServiceLayer::new(gate),
        }
    }

    /// Create a minimal dist dir with `manifest.json` mapping `/` → an
    /// `index.html` containing the marker text, and return the temp handle
    /// plus the dist path.
    fn build_cached_dist(marker: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).expect("create dist");
        std::fs::write(dist.join("index.html"), marker).expect("write index.html");
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            "/".to_owned(),
            crate::static_gen::ManifestEntry::new("index.html".to_owned()),
        );
        let manifest = crate::static_gen::StaticManifest {
            generated_at: "2026-06-14T00:00:00Z".to_owned(),
            autumn_version: "0.3.0".to_owned(),
            routes,
        };
        std::fs::write(
            dist.join("manifest.json"),
            serde_json::to_string(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");
        (tmp, dist)
    }

    fn ctx_with_static_gate(gate: crate::app::CustomLayerRegistration) -> RouterContext {
        RouterContext {
            exception_filters: Vec::new(),
            scoped_groups: Vec::new(),
            merge_routers: Vec::new(),
            nest_routers: Vec::new(),
            declared_routes: Vec::new(),
            custom_layers: Vec::new(),
            static_gate_layers: vec![gate],
            #[cfg(feature = "maud")]
            error_page_renderer: None,
            session_store: None,
            #[cfg(feature = "openapi")]
            openapi: None,
            #[cfg(feature = "mcp")]
            mcp: None,
        }
    }

    #[tokio::test]
    async fn static_gate_runs_before_cached_static_page() {
        // A cached SSG page exists at "/". The static_gate must intercept the
        // request BEFORE the static-first middleware serves the pre-rendered
        // HTML, redirecting unauthenticated visitors.
        let (_tmp, dist) = build_cached_dist("<h1>cached</h1>");
        let config = AutumnConfig::default();
        let ctx = ctx_with_static_gate(redirect_gate_registration());

        let app = super::try_build_router_with_static_inner(
            Vec::new(),
            &config,
            crate::state::AppState::for_test(),
            Some(dist.as_path()),
            ctx,
        )
        .expect("router builds");

        // Unauthenticated: gate fires before the cached page is served.
        let unauthed = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            unauthed.status(),
            StatusCode::FOUND,
            "static_gate must redirect before the cached page is served"
        );
        assert_eq!(
            unauthed.headers().get(http::header::LOCATION).unwrap(),
            "/login"
        );

        // Authenticated: gate passes through and the cached HTML is served.
        let authed = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("x-authed", "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authed.status(), StatusCode::OK);
        let body = axum::body::to_bytes(authed.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("cached"),
            "authenticated request should receive the cached page"
        );
    }

    #[tokio::test]
    async fn static_gate_runs_in_dynamic_mode() {
        // With no dist dir, the same gate must still run as the outermost
        // middleware so auth-gating code is portable across SSG and dynamic
        // modes.
        async fn dynamic_handler() -> &'static str {
            "dynamic"
        }
        let route = Route {
            method: http::Method::GET,
            path: "/",
            handler: axum::routing::get(dynamic_handler),
            name: "root",
            api_doc: crate::openapi::ApiDoc {
                method: "GET",
                path: "/",
                operation_id: "root",
                success_status: 200,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        };
        let config = AutumnConfig::default();
        let ctx = ctx_with_static_gate(redirect_gate_registration());

        let app = super::try_build_router_with_static_inner(
            vec![route],
            &config,
            crate::state::AppState::for_test(),
            None,
            ctx,
        )
        .expect("router builds");

        let unauthed = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauthed.status(), StatusCode::FOUND);

        let authed = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("x-authed", "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authed.status(), StatusCode::OK);
        let body = axum::body::to_bytes(authed.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "dynamic");
    }

    #[tokio::test]
    async fn static_gate_redirect_carries_security_headers_ssg() {
        // A gate short-circuit (302) must still carry the framework security
        // headers — SecurityHeadersLayer wraps the gate in the SSG path.
        let (_tmp, dist) = build_cached_dist("<h1>cached</h1>");
        let config = AutumnConfig::default();
        let ctx = ctx_with_static_gate(redirect_gate_registration());

        let app = super::try_build_router_with_static_inner(
            Vec::new(),
            &config,
            crate::state::AppState::for_test(),
            Some(dist.as_path()),
            ctx,
        )
        .expect("router builds");

        let unauthed = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauthed.status(), StatusCode::FOUND);
        // X-Content-Type-Options: nosniff is applied by SecurityHeadersLayer by
        // default; its presence proves the layer wraps the gate's response.
        assert_eq!(
            unauthed
                .headers()
                .get("x-content-type-options")
                .expect("gate redirect must carry security headers"),
            "nosniff"
        );
    }

    #[tokio::test]
    async fn static_gate_redirect_carries_security_headers_dynamic() {
        // Same contract in fully-dynamic mode (no dist): SecurityHeadersLayer is
        // the framework's outermost layer, so a gate short-circuit still carries
        // HSTS/CSP/nosniff. Guards against the dynamic/SSG inconsistency.
        async fn dynamic_handler() -> &'static str {
            "dynamic"
        }
        let route = Route {
            method: http::Method::GET,
            path: "/",
            handler: axum::routing::get(dynamic_handler),
            name: "root",
            api_doc: crate::openapi::ApiDoc {
                method: "GET",
                path: "/",
                operation_id: "root",
                success_status: 200,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        };
        let config = AutumnConfig::default();
        let ctx = ctx_with_static_gate(redirect_gate_registration());

        let app = super::try_build_router_with_static_inner(
            vec![route],
            &config,
            crate::state::AppState::for_test(),
            None,
            ctx,
        )
        .expect("router builds");

        let unauthed = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauthed.status(), StatusCode::FOUND);
        assert_eq!(
            unauthed
                .headers()
                .get("x-content-type-options")
                .expect("dynamic gate redirect must carry security headers"),
            "nosniff"
        );
    }

    #[test]
    fn static_gate_layer_requires_fail_closed_idempotency() {
        // A static_gate (e.g. a JWT/auth layer) is an opaque app layer for
        // idempotency: it must force fail-closed replay so a cached mutation
        // can't be served to a different principal sharing an Idempotency-Key.
        let gate = vec![redirect_gate_registration()];
        assert!(super::custom_layers_require_fail_closed_idempotency(&gate));
        // An empty set requires no fail-closed behaviour.
        assert!(!super::custom_layers_require_fail_closed_idempotency(&[]));
    }

    // ----------------------------------------------------------------------
    // #2214: the ingress stack's middleware futures must stay unboxed
    // ----------------------------------------------------------------------

    /// The structural half of issue #2214's fix, in one assertion per converted
    /// middleware.
    ///
    /// Each of these layers used to be an `axum::middleware::from_fn`, whose
    /// `FromFn::call` must `Box::pin` the async block it generates — the block's
    /// type cannot be named, so there is nowhere else for it to live. That was
    /// one heap allocation per request per call site, 19.57% of every byte the
    /// `request_pipeline` benchmark allocated.
    ///
    /// A hand-rolled `tower::Service` fixes it only if its `Future` associated
    /// type is genuinely named all the way down. Writing one that still returns
    /// `Pin<Box<dyn Future>>` — the shape `SessionService`, `LogContextService`
    /// and `TrustedProxiesService` use — would satisfy every behavioural test in
    /// the suite while allocating exactly as much as before. This test is what
    /// makes that impossible to do by accident: the sibling allocation gate in
    /// `tests/config_alloc_gate.rs` pins the *total*, and this pins *where* the
    /// total comes from.
    ///
    /// What this pins precisely: that the associated `Future` type is not
    /// literally a `Pin<Box<dyn Future>>`. `std::any::type_name` prints a type's
    /// path and generic arguments, never its fields, so it cannot see a box
    /// hidden inside a variant of an otherwise-named future — which is exactly
    /// what `WebhookReplayCleanupFuture`'s rare `Releasing` branch is, and why
    /// that one is pinned by the sibling test below (on size) instead of here.
    #[test]
    fn converted_ingress_middleware_futures_are_never_boxed() {
        /// The inner service every layer under test is stacked on: its own
        /// future is a named `Ready`, so any `Box` in the resulting type name
        /// was contributed by the layer.
        type Inner = tower::util::ServiceFn<
            fn(
                axum::extract::Request,
            )
                -> std::future::Ready<Result<axum::response::Response, std::convert::Infallible>>,
        >;
        type Fut<Svc> = <Svc as tower::Service<axum::extract::Request>>::Future;

        fn assert_unboxed<Svc: tower::Service<axum::extract::Request>>(what: &str) {
            let name = std::any::type_name::<Fut<Svc>>();
            assert!(
                !name.contains("Box"),
                "{what} must return a named, unboxed future — an \
                 `axum::middleware::from_fn`-shaped `Box::pin` here is one heap \
                 allocation on every request (issue #2214). Got: {name}"
            );
        }

        assert_unboxed::<super::TrustedHostService<Inner>>("TrustedHostService");
        assert_unboxed::<super::StartupBarrierService<Inner>>("StartupBarrierService");
        assert_unboxed::<super::RequestTimeoutService<Inner>>("RequestTimeoutService");
        assert_unboxed::<crate::assets::AssetCacheControlService<Inner>>(
            "AssetCacheControlService",
        );
        assert_unboxed::<crate::events::EventAppContextService<Inner>>("EventAppContextService");
        assert_unboxed::<crate::read_your_writes::ReadYourWritesService<Inner>>(
            "ReadYourWritesService",
        );
        assert_unboxed::<crate::middleware::method_override::MethodOverrideRejectionService<Inner>>(
            "MethodOverrideRejectionService",
        );
        #[cfg(feature = "oauth2")]
        assert_unboxed::<super::HttpInterceptorService<Inner>>("HttpInterceptorService");
    }

    /// `WebhookReplayCleanupFuture` is the one converted middleware that still
    /// boxes, and that is deliberate: releasing the replay keys a failed webhook
    /// delivery registered is genuinely async work that has to run *after* the
    /// inner future resolves, so it cannot be folded into the inner service's
    /// own future.
    ///
    /// The box lives in the `Releasing` variant, which is only ever constructed
    /// for a `5xx` response that actually registered keys — so the happy path
    /// (and every request that is not a webhook delivery at all) never takes it.
    /// The sibling test above cannot pin that, because `type_name` does not
    /// print field types; this one does, by size. `Serving` stores the scoped
    /// inner future, the replay cell and the drop guard **inline**, so the enum
    /// must be at least as large as all three together. Move the box to
    /// `Serving` and the enum collapses to roughly a pointer plus the response,
    /// and this fails.
    #[test]
    fn webhook_replay_cleanup_boxes_only_its_rare_release_branch() {
        use std::mem::size_of;

        type Ready = std::future::Ready<Result<axum::response::Response, std::convert::Infallible>>;
        type Scoped = tokio::task::futures::TaskLocalFuture<crate::webhook::ReplayStoreCell, Ready>;

        let serving_inline = size_of::<Scoped>()
            + size_of::<crate::webhook::ReplayStoreCell>()
            + size_of::<crate::webhook::ReplayKeyGuard>();
        let whole = size_of::<crate::webhook::WebhookReplayCleanupFuture<Ready>>();
        assert!(
            whole >= serving_inline,
            "WebhookReplayCleanupFuture must hold the scoped inner future, the \
             replay cell and the drop guard inline in its `Serving` variant \
             ({serving_inline} bytes together), but the whole future is only \
             {whole} bytes — the inner future has been boxed, which costs an \
             allocation on every request rather than only on a failed webhook \
             delivery (issue #2214)"
        );
    }

    /// The startup barrier's short-circuit branch: until the app reports startup
    /// complete, a request to a non-exempt path is refused with `503`.
    ///
    /// Driven directly over a stub inner service because `apply_startup_barrier`
    /// is production-only — `TestApp::build` deliberately mirrors just its two
    /// response-side fallbacks (see `crate::test`), so no end-to-end test in the
    /// suite can reach this branch.
    #[tokio::test]
    async fn startup_barrier_refuses_requests_until_startup_completes() {
        use tower::{Layer, ServiceExt};

        fn ok_service() -> impl tower::Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone {
            tower::service_fn(|_req: axum::extract::Request| async move {
                Ok::<_, std::convert::Infallible>("handler ran".into_response())
            })
        }

        // `#[allow(future_not_send)]`: `tower::service_fn`'s closure is not
        // `Sync`, so this helper's future is `!Send`. It only ever runs on a
        // `#[tokio::test]` current-thread runtime, which never moves it.
        #[allow(clippy::future_not_send)]
        async fn status_for(startup_complete: bool, path: &str) -> (StatusCode, String) {
            let state = AppState::for_test().with_startup_complete(startup_complete);
            let config = AutumnConfig::default();
            let layer = super::StartupBarrierLayer::new(super::StartupBarrierState::from_config(
                &config, &state,
            ));
            let response = layer
                .layer(ok_service())
                .oneshot(
                    axum::extract::Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("request builds"),
                )
                .await
                .expect("infallible");
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("body collects");
            (status, String::from_utf8_lossy(&body).into_owned())
        }

        let (status, body) = status_for(false, "/anything").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "Service is still starting up");

        // Control: the same request once startup has completed reaches the
        // handler, so the 503 above is the barrier and not a broken stub.
        let (status, body) = status_for(true, "/anything").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "handler ran");

        // Probe/actuator paths are exempt even before startup completes, so a
        // platform readiness check can still reach them.
        let (status, _) = status_for(false, "/actuator/health").await;
        assert_ne!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "probe paths must bypass the startup barrier"
        );
    }
}
#[derive(Clone, Debug)]
pub struct TrustedHostPolicy {
    rules: Arc<Vec<String>>,
    allow_any: bool,
    allow_missing_host: bool,
    probe_bypass_paths: Arc<std::collections::HashSet<String>>,
}

impl TrustedHostPolicy {
    pub fn from_config(config: &AutumnConfig) -> Self {
        let mut rules: Vec<String> = config
            .security
            .trusted_hosts
            .hosts
            .iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .map(|h| h.trim_end_matches('.').to_owned())
            .filter(|h| !h.is_empty())
            .collect();
        let is_production = matches!(config.profile.as_deref(), Some("prod" | "production"));
        if !is_production {
            rules.extend(
                ["localhost", "127.0.0.1", "::1"]
                    .into_iter()
                    .map(std::borrow::ToOwned::to_owned),
            );
        }
        let allow_any = rules.iter().any(|h| h == "*");
        let probe_bypass_paths = probe_bypass_paths(config).into_iter().collect();
        Self {
            rules: Arc::new(rules),
            allow_any,
            allow_missing_host: !is_production,
            probe_bypass_paths: Arc::new(probe_bypass_paths),
        }
    }

    /// Whether a request carrying no usable `Host` is allowed through. Mirrors
    /// `trusted_host_rejection`'s missing-host branch for callers (e.g. the MCP
    /// envelope) that enforce the policy outside [`TrustedHostService`].
    ///
    /// Only the `mcp` feature consumes this today; gated so default-feature
    /// builds don't flag it as dead code.
    #[cfg(feature = "mcp")]
    pub const fn allows_missing_host(&self) -> bool {
        self.allow_missing_host
    }

    pub fn allows_host(&self, host: &str) -> bool {
        if self.allow_any {
            return true;
        }
        self.rules.iter().any(|rule| {
            rule.strip_prefix('.').map_or_else(
                || host == rule,
                |suffix| {
                    host == suffix
                        || host
                            .strip_suffix(suffix)
                            .is_some_and(|prefix| prefix.ends_with('.'))
                },
            )
        })
    }
}

/// Metadata carrying API version, sunset opt-out, and security configuration for a route.
#[derive(Clone, Debug)]
pub struct RouteVersionMetadata {
    pub version: String,
    pub sunset_opt_out: bool,
    pub secured: bool,
    pub required_roles: &'static [&'static str],
    pub has_policy: bool,
}

/// Middleware that handles API deprecation, sunsets, and Gone responses.
async fn api_versioning_middleware(
    state: axum::extract::State<AppState>,
    route_version: Option<axum::extract::Extension<RouteVersionMetadata>>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(axum::extract::Extension(meta)) = route_version else {
        return next.run(request).await;
    };

    let clock = state.clock();
    let now = clock.now();

    let versions = state.extension::<crate::app::RegisteredApiVersions>();
    let matching_version = versions
        .as_ref()
        .and_then(|v| v.0.iter().find(|av| av.version == meta.version));

    let Some(version) = matching_version else {
        return next.run(request).await;
    };

    let is_deprecated = version.deprecated_at.is_some_and(|d| now >= d);
    let is_sunset = version.sunset_at.is_some_and(|s| now >= s);

    if is_sunset && !meta.sunset_opt_out {
        if meta.has_policy {
            return next.run(request).await;
        }
        if meta.secured {
            let session = request.extensions().get::<crate::session::Session>();
            let mut auth_failed = false;
            let mut auth_error = None;
            if let Some(session) = session {
                if let Err(err) = crate::auth::__check_secured_with_key(
                    session,
                    state.auth_session_key(),
                    meta.required_roles,
                )
                .await
                {
                    auth_failed = true;
                    auth_error = Some(err);
                }
            } else {
                auth_failed = true;
                auth_error = Some(crate::error::AutumnError::unauthorized_msg(
                    "authentication required",
                ));
            }
            if auth_failed {
                return auth_error.unwrap().into_response();
            }
        }

        let err = crate::error::AutumnError::gone_msg(format!(
            "API version '{}' has been sunsetted.",
            meta.version
        ));
        let mut response = err.into_response();
        if let Some(sunset) = version.sunset_at {
            let http_date = sunset.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
            if let Ok(val) = axum::http::HeaderValue::from_str(&http_date) {
                response.headers_mut().insert("Sunset", val);
            }
        }
        let deprecation_date = match (version.deprecated_at, version.sunset_at) {
            (Some(d), Some(s)) => Some(d.min(s)),
            (d, s) => d.or(s),
        };
        if let Some(date) = deprecation_date {
            let timestamp = date.timestamp();
            if let Ok(val) = axum::http::HeaderValue::from_str(&format!("@{timestamp}")) {
                response.headers_mut().insert("Deprecation", val);
            }
        }
        return response;
    }

    let mut response = next.run(request).await;

    if is_deprecated || is_sunset {
        let deprecation_date = match (version.deprecated_at, version.sunset_at) {
            (Some(d), Some(s)) => Some(d.min(s)),
            (d, s) => d.or(s),
        };
        if let Some(date) = deprecation_date {
            let timestamp = date.timestamp();
            if let Ok(val) = axum::http::HeaderValue::from_str(&format!("@{timestamp}")) {
                response.headers_mut().insert("Deprecation", val);
            }
        }
    }
    if let Some(sunset) = version.sunset_at.filter(|_| is_deprecated || is_sunset) {
        let http_date = sunset.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        if let Ok(val) = axum::http::HeaderValue::from_str(&http_date) {
            response.headers_mut().insert("Sunset", val);
        }
    }

    response
}

/// Helper function to perform a sunset check during dynamic handler execution.
/// Returns a `410 Gone` response if the route version has sunsetted.
#[must_use]
pub fn check_sunset(
    state: &crate::state::AppState,
    meta: &RouteVersionMetadata,
) -> Option<axum::response::Response> {
    let clock = state.clock();
    let now = clock.now();

    let versions = state.extension::<crate::app::RegisteredApiVersions>();
    let matching_version = versions
        .as_ref()
        .and_then(|v| v.0.iter().find(|av| av.version == meta.version));

    let version = matching_version?;
    let is_sunset = version.sunset_at.is_some_and(|s| now >= s);

    if is_sunset && !meta.sunset_opt_out {
        let err = crate::error::AutumnError::gone_msg(format!(
            "API version '{}' has been sunsetted.",
            meta.version
        ));
        let mut response = axum::response::IntoResponse::into_response(err);
        if let Some(sunset) = version.sunset_at {
            let http_date = sunset.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
            if let Ok(val) = axum::http::HeaderValue::from_str(&http_date) {
                response.headers_mut().insert("Sunset", val);
            }
        }
        let deprecation_date = match (version.deprecated_at, version.sunset_at) {
            (Some(d), Some(s)) => Some(d.min(s)),
            (d, s) => d.or(s),
        };
        if let Some(date) = deprecation_date {
            let timestamp = date.timestamp();
            if let Ok(val) = axum::http::HeaderValue::from_str(&format!("@{timestamp}")) {
                response.headers_mut().insert("Deprecation", val);
            }
        }
        return Some(response);
    }

    None
}

#[cfg(all(test, feature = "htmx"))]
mod idiomorph_tests {
    use super::*;
    use http::StatusCode;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn idiomorph_handler_returns_js_with_correct_headers() {
        let response = idiomorph_handler().await;

        assert_eq!(response.status(), StatusCode::OK);

        let ct = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(ct, "application/javascript");

        let cc = response
            .headers()
            .get(http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // The idiomorph URL is not content-fingerprinted, so the response must
        // revalidate rather than advertise a year-long `immutable` cache. This
        // guards against returning clients running a stale copy after the
        // vendored bytes change.
        assert!(
            cc.contains("must-revalidate"),
            "expected revalidating cache-control, got: {cc}"
        );
        assert!(
            !cc.contains("immutable"),
            "cache-control must not be immutable for a non-fingerprinted URL, got: {cc}"
        );

        // A weak, content-derived ETag lets caches revalidate (and pick up new
        // bytes when the script changes). It is weak rather than strong because
        // compression middleware may re-encode this response after the handler
        // attaches the validator, so the identity/gzip/br variants share a tag
        // despite differing byte streams.
        let etag = response
            .headers()
            .get(http::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            etag.starts_with("W/\"idiomorph-") && etag.ends_with('"'),
            "expected a weak quoted idiomorph ETag, got: {etag}"
        );

        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!body.is_empty(), "idiomorph JS body must be non-empty");
    }
}

#[cfg(test)]
mod proptests {
    //! Property-based invariants for the low-level path/host string helpers.
    //! These are `pub(crate)` (only reachable via the `cfg(fuzzing)` seam
    //! module), so they are exercised here in-crate rather than from an
    //! integration test.
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Mounting the root child (`"/"` or empty) is the identity on a
        /// non-empty prefix, and idempotent: re-mounting the root child on the
        /// result leaves it unchanged. (An empty prefix collapses to `"/"`.)
        #[test]
        fn join_nested_path_root_child_is_identity(prefix in "/?[a-z0-9/]{0,20}", root in prop::sample::select(vec!["/", ""])) {
            let once = join_nested_path(&prefix, root);
            let expected = if prefix.is_empty() { "/".to_owned() } else { prefix };
            prop_assert_eq!(&once, &expected);
            let twice = join_nested_path(&once, root);
            prop_assert_eq!(once, twice);
        }

        /// `join_nested_path` never introduces a doubled slash at the join seam
        /// for well-formed single-segment children.
        #[test]
        fn join_nested_path_no_double_slash_at_seam(prefix in "/[a-z0-9]{1,8}/?", child in "/[a-z0-9]{1,8}") {
            let joined = join_nested_path(&prefix, &child);
            prop_assert!(!joined.contains("//"), "unexpected `//` in {joined:?}");
        }

        /// `extract_host_without_port` never panics on arbitrary input and,
        /// when it returns something, that something is a substring of the
        /// trimmed input (it only ever strips a port / brackets, never invents
        /// characters).
        #[test]
        fn extract_host_without_port_never_panics(header in ".*") {
            if let Some(host) = extract_host_without_port(&header) {
                prop_assert!(header.contains(host));
            }
        }

        /// `path_matches_route_prefix` never panics and is reflexive: a path
        /// always matches itself as a prefix.
        #[test]
        fn path_matches_route_prefix_reflexive(path in ".*") {
            prop_assert!(path_matches_route_prefix(&path, &path));
        }

        /// `path_matches_route_prefix` is consistent with its documented
        /// contract: a match means either exact equality or a `/`-delimited
        /// boundary immediately after the prefix.
        #[test]
        fn path_matches_route_prefix_boundary(path in "/?[a-z0-9/]{0,24}", prefix in "/?[a-z0-9/]{0,24}") {
            if path_matches_route_prefix(&path, &prefix) {
                let boundary_ok = path == prefix
                    || path.strip_prefix(&prefix).is_some_and(|rest| rest.starts_with('/'));
                prop_assert!(boundary_ok, "match without boundary: path={path:?} prefix={prefix:?}");
            }
        }
    }

    // `extract_path_params` (openapi-only) never panics on arbitrary input and
    // only ever returns non-empty, brace-free parameter names.
    #[cfg(feature = "openapi")]
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn extract_path_params_never_panics(path in ".*") {
            for name in extract_path_params(&path) {
                prop_assert!(!name.is_empty());
                let has_brace = name.contains('{') || name.contains('}');
                prop_assert!(!has_brace, "param name should be brace-free: {name:?}");
            }
        }

        /// Brace-dense variant of the invariant above. `.*` makes brace
        /// characters astronomically rare, so unbalanced-brace inputs like
        /// `"{{}"` (the #1721 regression) only surface via lucky CI seeds. This
        /// strategy draws exclusively from brace/colon/letter characters so
        /// malformed braces are exercised on nearly every case, and a committed
        /// regression seed (proptest-regressions/router.txt) pins a brace-dense
        /// input (which replays to `"{{iw:}"` under this strategy) that trips the
        /// pre-fix brace-in-name bug deterministically.
        #[test]
        fn extract_path_params_brace_inputs_are_brace_free(path in "[{}a-z:]{0,6}") {
            for name in extract_path_params(&path) {
                prop_assert!(!name.is_empty());
                let has_brace = name.contains('{') || name.contains('}');
                prop_assert!(!has_brace, "param name should be brace-free for {path:?}: {name:?}");
            }
        }
    }
}
