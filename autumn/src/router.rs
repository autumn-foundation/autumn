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
use crate::extract::State;
use crate::idempotency::{IdempotencyLayer, IdempotencyStore, MemoryIdempotencyStore};
use crate::middleware::RequestIdLayer;
use crate::middleware::dev;
use crate::middleware::exception_filter::{
    ExceptionFilter, ExceptionFilterLayer, ProblemDetailsFilter,
};
use crate::route::Route;
use crate::state::AppState;
use axum::middleware::Next;
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
    /// When `Some`, [`apply_session_layer`](crate::session::apply_session_layer)
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
        // The mount path must be a single static endpoint: reject empty,
        // non-absolute, doubled-slash, and dynamic (`{capture}` / `{*rest}`)
        // paths so MCP cannot shadow a whole path class and so the exact-path
        // collision preflight reserves the concrete URL it actually matches.
        // Colon-prefixed segments (`/:mcp`, axum 0.7 capture syntax) are also
        // rejected: axum 0.8's `Router::route` panics on them during assembly
        // (`validate_v07_paths`), so catching them here yields the recoverable
        // `InvalidMcpPath` error instead of a startup crash.
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
    let route_timeouts = build_route_timeout_table(&route_list, &ctx.scoped_groups);

    let idempotency_layers = build_idempotency_layers(config, state)?;
    // Both `.layer(..)` custom layers and `.static_gate(..)` gate layers are
    // opaque app layers for idempotency: an auth/tenant layer in either slot
    // must force fail-closed replay so a cached mutation can't be served to a
    // different principal carrying the same Idempotency-Key.
    let opaque_app_layers_present = opaque_app_layers_override.unwrap_or_else(|| {
        custom_layers_require_fail_closed_idempotency(&ctx.custom_layers)
            || custom_layers_require_fail_closed_idempotency(&ctx.static_gate_layers)
    });
    let mut router = group_and_mount_routes(
        route_list,
        idempotency_layers.as_ref(),
        opaque_app_layers_present,
        state,
    );

    let dev_reload_enabled = dev::is_enabled_with_env(&crate::config::OsEnv);

    router = mount_framework_routes(router, config, dev_reload_enabled);

    let (mounted_probe_paths, router_with_probes) = mount_probe_endpoints(router, config);
    router = router_with_probes;

    router = mount_actuator_endpoints(router, config, &mounted_probe_paths)?;

    #[cfg(feature = "openapi")]
    if let Some(openapi_router) = openapi_router {
        router = router.merge(openapi_router);
    }

    // Static file serving. Fingerprinted assets (e.g. `autumn.a1b2c3d4.css`)
    // are served with `Cache-Control: public, max-age=31536000, immutable`; all
    // other static files use the default browser policy.
    //
    // When the app embedded its `static/` tree (feature = "embed-assets" plus a
    // registered dir), serve `/static/*` from the binary — no disk read, no
    // sidecar directory. Otherwise serve from the project's `static/` directory
    // on disk (the dev default, preserving hot-reload).
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
    router = router.layer(axum::middleware::from_fn(
        crate::assets::asset_cache_control,
    ));

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
    )?;

    if dev_reload_enabled {
        router = router
            .layer(axum::middleware::from_fn(dev::disable_static_cache))
            .layer(axum::middleware::from_fn(dev::inject_live_reload));
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
    let router = router.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        http_interceptor_middleware,
    ));

    // Install the request's app as the ambient event-bus context so any code in
    // the request (handlers, services) that calls the free `events::publish`
    // dispatches against this app rather than the process-global bus — keeping
    // parallel in-process apps (notably tests) isolated.
    let router = router.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        event_app_context_middleware,
    ));

    // Mount the MCP endpoint last so its dispatch target — a clone of the
    // fully-assembled router with state applied — traverses the exact same
    // routes, layers, and middleware an HTTP request would. The clone is
    // taken *before* the MCP route is added, so `tools/call` never recurses
    // into the MCP endpoint itself.
    //
    // `static_gate` is intentionally NOT in this dispatch clone, in EITHER mode:
    // the gate layers are applied after this clone is taken (below, after the MCP
    // merge, in the fully-dynamic path; outside the static-first middleware in the
    // SSG/ISG path). A `static_gate` is a page-cache gate whose only action is a
    // browser redirect/reject, which is meaningless for a JSON-RPC `tools/call`.
    // MCP/API auth belongs in route-level guards / `#[secured]` / session, which
    // DO traverse this clone.
    //
    // KNOWN LIMITATION (static/ISR mode): when an app has a `dist` manifest,
    // `try_build_router_with_static_inner` also drains the global custom layers
    // (`AppBuilder::layer`) and applies them outside the static-first middleware,
    // after this clone is taken. So in static mode a `tools/call` replay does not
    // pass through hand-rolled global `.layer(...)` middleware (it would in the
    // fully-dynamic path, where custom layers are applied via `apply_middleware`
    // before the clone). Restoring full parity for custom layers would require
    // making the appliers re-usable (they are `FnOnce` today), so this is left
    // documented rather than fixed for that narrow combination.
    #[cfg(feature = "mcp")]
    let router = if let Some((mount_path, tools, endpoint_layer)) = mcp_prepared {
        // The framework's outermost `SecurityHeadersLayer` is applied AFTER this
        // clone (below, with the gate), so the dispatch snapshot would otherwise
        // miss it. That layer also injects `CspNonce` into request extensions, so
        // without it a `tools/call` replay of a handler using the `CspNonce`
        // extractor would 500 when `csp_nonce` is enabled. Re-attach it to the
        // dispatch clone only: a direct HTTP request gets the same layer via the
        // outer application, and the replay's response headers are discarded when
        // `serve_mcp` rebuilds the JSON-RPC envelope, so there is no duplicate
        // live header. (The gate is intentionally NOT re-attached here — a browser
        // redirect/reject is meaningless for JSON-RPC dispatch.)
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
        };
        let mut mcp_router =
            crate::mcp::build_mcp_router(&mount_path, tools, dispatch, wiring, endpoint_layer);
        // NOTE: the inbound request-timeout layer for this envelope is applied
        // further down, *outer* to the rate-limit layer (search for
        // `apply_request_timeout_middleware` below). It must wrap the limiter so a
        // stalled Redis rate-limit decision is bounded by `request_timeout_ms`,
        // matching the main stack where `apply_middleware` installs the timeout
        // outer to `apply_rate_limit_middleware`.
        // Gate the envelope under maintenance mode, mirroring the layer
        // `apply_middleware` installs for direct routes. The `/mcp` router is
        // merged after that layer, so without this `initialize`/`tools/list`
        // would keep serving the tool catalog during maintenance (the
        // `tools/call` replay is already gated — the dispatch clone carries the
        // layer). Applied before the `TrustedProxiesLayer` below so it is inner
        // to it: the maintenance IP allow-list then reads the proxy-resolved
        // identity, exactly as the direct-route layer does, instead of a
        // spoofable raw `X-Forwarded-For`.
        mcp_router = mcp_router.layer(build_maintenance_layer(config, state));
        // Stamp `ResolvedClientIdentity` on the *outer* `/mcp` request too. The
        // MCP route is merged after `apply_middleware`, so the centralized
        // `TrustedProxiesLayer` above does not wrap it; without this, the
        // endpoint's own DNS-rebinding / same-origin check would fall back to
        // the raw (possibly proxy-rewritten) `Host` and wrongly 403 a
        // same-origin browser client behind a TLS-terminating proxy. The
        // dispatch clone already carries its own copy of this layer.
        mcp_router = apply_trusted_proxies_middleware(mcp_router, config);
        // The MCP route is merged after `apply_upload_middleware`, so axum's
        // built-in 2 MiB `DefaultBodyLimit` — not the app's configured limit —
        // would otherwise govern the `tools/call` envelope's `Bytes` body. Apply
        // the same cap a direct JSON endpoint gets so larger-but-valid tool
        // payloads aren't rejected before dispatch.
        mcp_router = mcp_router.layer(axum::extract::DefaultBodyLimit::max(
            config.security.upload.max_request_size_bytes,
        ));
        // Rate-limit the envelope so `secure_mcp` auth rejections — which never
        // reach the dispatch clone's limiter — are throttled (credential
        // guessing otherwise consumes no per-client bucket). A successful
        // tools/call is counted once here and replayed with `RateLimitExempt`,
        // so it isn't double-counted by the dispatch pipeline's own limiter.
        // No-op when rate limiting is disabled (matching `envelope_rate_limited`).
        //
        // KNOWN LIMITATION (key_strategy = AuthenticatedPrincipal + session
        // auth): the envelope keys on the IP fallback because the session layer
        // — which `populate_rate_limit_principal` reads the principal from — is
        // applied inside `apply_middleware` and does not wrap this late-merged
        // router, so no `RateLimitPrincipal` is resolved here. Because the
        // tools/call replay is then exempted, the dispatch clone's
        // principal-aware limiter is skipped too, so a session-authenticated MCP
        // call does not consume the same per-user bucket a direct request would
        // (the framework only derives `RateLimitPrincipal` from the session).
        mcp_router = apply_rate_limit_middleware(mcp_router, config, state);
        // Bound the whole envelope — the rate-limit decision (a stalled
        // Redis-backed limiter would otherwise tie up `/mcp` indefinitely), the
        // metadata/auth work (initialize, tools/list, and `secure_mcp` auth
        // rejections that never reach the dispatch clone), and the in-process
        // `tools/call` dispatch — by the global inbound deadline. The `/mcp`
        // router is merged after `apply_middleware`, so the timeout layer
        // installed there does NOT wrap it; without this the prod global deadline
        // would not bound this surface. Applied here, outer to the rate-limit
        // layer above (matching the main stack, where `apply_middleware` installs
        // the timeout outer to `apply_rate_limit_middleware`) but inner to the
        // security-header and CORS layers below, so a stalled limiter is bounded
        // while the timeout 503 still flows out through those layers and stays
        // CORS-readable. Route-level overrides do not apply to the fixed mount
        // path, so an empty override table is passed (the layer is a no-op when
        // the global timeout is disabled).
        //
        // KNOWN LIMITATION (tools/call vs per-route timeout): this envelope timer
        // wraps the whole POST, including the in-process `tools/call` dispatch
        // replay, with the global default deadline. The dispatch clone carries
        // its own per-route timeout layer, but it is *inner* to this one, so a
        // tool whose route declares `timeout = "off"` or a longer `timeout_ms`
        // is still capped at the global default when invoked via MCP (it runs
        // unbounded / longer over a direct HTTP call). Honoring the per-route
        // policy here would require propagating the dispatched route's timeout
        // out to this single fixed-path endpoint, which has no per-route
        // distinction at the layer level; the global deadline is kept as a
        // safety bound instead. `mirror_cors = false`: the 503 already flows out
        // through this router's own (outer) `CorsLayer` from `apply_mcp_cors_layer`.
        mcp_router = apply_request_timeout_middleware(
            mcp_router,
            config,
            state.metrics.clone(),
            std::sync::Arc::new(std::collections::HashMap::new()),
            false,
        );
        // Security headers (HSTS/CSP/etc.), mirroring the `SecurityHeadersLayer`
        // `apply_middleware` installs for direct routes. The `/mcp` router is
        // merged after that layer, so without this the envelope's responses —
        // `initialize`/`tools/list`, auth 401/403, and rate-limit 429 — would
        // ship without the configured `security.headers` every direct route
        // carries. (The `tools/call` replay's headers are produced on the
        // dispatch clone and discarded when `serve_mcp` rebuilds the JSON-RPC
        // response, so the envelope needs its own copy.)
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

    // Apply the pre-static gate and the framework's outermost `SecurityHeadersLayer`
    // LAST, after the MCP dispatch clone above was taken. This keeps the gate out
    // of the `tools/call` dispatch path in fully-dynamic mode (matching the SSG/ISG
    // path and the documented intent that a browser redirect/reject is meaningless
    // for JSON-RPC dispatch), while still running the gate before session and the
    // static cache for ordinary HTTP requests. `SecurityHeadersLayer` is applied
    // outermost so a gate redirect/401 short-circuit still carries HSTS/CSP/nosniff;
    // a single application keeps CSP nonces consistent.
    //
    // In the SSG/ISG path `defer_security_headers` is true and the gate layers were
    // drained by `try_build_router_with_static_inner` (which applies both the gate
    // and the single outer `SecurityHeadersLayer` outside the static-first
    // middleware), so this block is a no-op there.
    let router = if defer_security_headers {
        router
    } else {
        let router =
            apply_layers_in_registration_order(router, static_gate_layers, "Pre-static gate");
        router.layer(crate::security::SecurityHeadersLayer::from_config(
            &config.security.headers,
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
        let Some(end_rel) = after_brace.find('}') else {
            break;
        };

        let inner = &after_brace[..end_rel];
        let name = inner.split(':').next().unwrap_or(inner).trim();
        if !name.is_empty() {
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
    // Framework-mounted GETs.
    claimed.insert(config.health.path.clone());
    claimed.insert(config.health.live_path.clone());
    claimed.insert(config.health.ready_path.clone());
    claimed.insert(config.health.startup_path.clone());
    for path in crate::actuator::actuator_endpoint_paths(
        &config.actuator.prefix,
        config.actuator.sensitive,
        config.actuator.prometheus,
    ) {
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
        claimed.insert(config.dev.inspector_path.clone());
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
/// `/v3/api-docs`). We surface that as a recoverable
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
) -> (std::collections::HashSet<String>, axum::Router<S>)
where
    S: Clone + Send + Sync + 'static,
    AppState: axum::extract::FromRef<S>,
{
    // Probe endpoints (auto-mounted)
    let mut mounted_probe_paths = std::collections::HashSet::new();

    if mounted_probe_paths.insert(config.health.live_path.clone()) {
        router = router.route(
            &config.health.live_path,
            axum::routing::get(crate::probe::live_handler::<AppState>),
        );
    }
    if mounted_probe_paths.insert(config.health.ready_path.clone()) {
        router = router.route(
            &config.health.ready_path,
            axum::routing::get(crate::probe::ready_handler::<AppState>),
        );
    }
    if mounted_probe_paths.insert(config.health.startup_path.clone()) {
        router = router.route(
            &config.health.startup_path,
            axum::routing::get(crate::probe::startup_handler::<AppState>),
        );
    }
    if mounted_probe_paths.insert(config.health.path.clone()) {
        router = router.route(
            &config.health.path,
            axum::routing::get(crate::health::handler::<AppState>),
        );
    }
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
            let mut handler = route.handler;
            if let Some(layer) = selected_layer {
                handler = handler.layer(layer.clone());
            }
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

fn apply_compression_middleware<S>(
    mut router: axum::Router<S>,
    config: &AutumnConfig,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    if config.compression.enabled {
        use tower_http::compression::predicate::{DefaultPredicate, NotForContentType, Predicate};
        // Extend the default predicate (skips images, gRPC, SSE, small bodies) to also
        // skip binary media and already-compressed formats — compressing these wastes
        // CPU, increases transfer size for archives, and can confuse media players.
        let predicate = DefaultPredicate::new()
            // Binary media — already-encoded by codec, not compressible by gzip/br.
            .and(NotForContentType::const_new("audio/"))
            .and(NotForContentType::const_new("video/"))
            .and(NotForContentType::const_new("application/octet-stream"))
            // Compressed archive formats — re-compressing wastes CPU.
            .and(NotForContentType::const_new("application/zip"))
            .and(NotForContentType::const_new("application/gzip"))
            .and(NotForContentType::const_new("application/x-gzip"))
            .and(NotForContentType::const_new("application/zstd"))
            .and(NotForContentType::const_new("application/x-bzip2"))
            .and(NotForContentType::const_new("application/x-bzip"))
            .and(NotForContentType::const_new("application/x-rar-compressed"))
            .and(NotForContentType::const_new("application/vnd.rar"))
            .and(NotForContentType::const_new("application/x-7z-compressed"));
        router =
            router.layer(tower_http::compression::CompressionLayer::new().compress_when(predicate));
        tracing::info!("Response compression enabled (gzip/brotli)");
    }
    router
}

fn apply_cors_middleware<S>(mut router: axum::Router<S>, config: &AutumnConfig) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    // CORS middleware (only applied when allowed_origins is non-empty)
    if !config.cors.allowed_origins.is_empty() {
        let cors = build_cors_layer(&config.cors);
        tracing::info!(
            origins = ?config.cors.allowed_origins,
            credentials = config.cors.allow_credentials,
            "CORS enabled"
        );
        router = router.layer(cors);
    }
    router
}

fn apply_csrf_middleware<S>(
    mut router: axum::Router<S>,
    config: &AutumnConfig,
    signing_keys: Option<std::sync::Arc<crate::security::config::ResolvedSigningKeys>>,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    // CSRF middleware (only applied when enabled)
    if config.security.csrf.enabled {
        let mut csrf_layer = crate::security::CsrfLayer::from_config(&config.security.csrf)
            .with_max_scan_bytes(config.security.upload.max_request_size_bytes);
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
        router = router.layer(csrf_layer);
    }
    router
}

fn apply_bot_protection_middleware<S>(
    mut router: axum::Router<S>,
    config: &AutumnConfig,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
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
        router = router.layer(layer);
    }
    router
}

async fn populate_rate_limit_principal(
    axum::extract::State(state): axum::extract::State<AppState>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Populate RateLimitPrincipal from the *verified* session identity only.
    //
    // We deliberately do NOT fall back to a raw Authorization header here: this
    // shim runs as a global layer outer to route-scoped auth (RequireApiToken),
    // so any bearer token visible at this point is still unverified and fully
    // attacker-controlled. Keying the limiter on it would let a caller rotate
    // the token to mint unlimited buckets (defeating the per-IP fallback) or
    // forge another user's principal to exhaust their bucket. When no verified
    // principal is available, the limiter's extract_key falls back to IP keying,
    // which is the correct safe default. API-token routes that want
    // per-principal limiting should place a RateLimitLayer inner to
    // RequireApiToken, which sets the verified principal ID (see
    // RequireApiTokenService::call).
    if let Some(session) = req.extensions().get::<crate::session::Session>() {
        let auth_session_key = state.auth_session_key();
        if let Some(user_id) = session.get(auth_session_key).await {
            req.extensions_mut()
                .insert(crate::security::RateLimitPrincipal(user_id));
        }
    }
    next.run(req).await
}

fn apply_trusted_proxies_middleware<S>(
    router: axum::Router<S>,
    config: &AutumnConfig,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let tp = &config.security.trusted_proxies;
    let layer = crate::security::TrustedProxiesLayer::from_config(tp);
    if tp.trust_forwarded_headers || !tp.ranges.is_empty() || tp.trusted_hops.is_some() {
        tracing::info!(
            ranges = ?tp.ranges,
            trusted_hops = ?tp.trusted_hops,
            "Centralized trusted-proxy resolution enabled"
        );
    }
    router.layer(layer)
}

fn apply_rate_limit_middleware(
    mut router: axum::Router<AppState>,
    config: &AutumnConfig,
    state: &AppState,
) -> axum::Router<AppState> {
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
        // envelope limiter (both built here), so it honors `RateLimitExempt` to
        // avoid double-counting an already-charged `tools/call`. User-installed
        // limiters don't, so MCP replays still consume their per-route buckets.
        let mut layer = crate::security::RateLimitLayer::from_config(rl).honoring_mcp_exempt();
        if has_top_level_proxy_config && !has_rate_limit_proxy_config {
            let resolver = crate::security::ProxyResolver::from_config(tp);
            layer = layer.with_proxy_resolver(resolver);
        }
        tracing::info!(
            rps = config.security.rate_limit.requests_per_second,
            burst = config.security.rate_limit.burst,
            "Rate limiting enabled"
        );
        router = router.layer(layer);

        if config.security.rate_limit.key_strategy
            == crate::security::KeyStrategy::AuthenticatedPrincipal
        {
            router = router.layer(axum::middleware::from_fn_with_state(
                state.clone(),
                populate_rate_limit_principal,
            ));
        }
    }
    router
}

fn apply_upload_middleware<S>(router: axum::Router<S>, config: &AutumnConfig) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let upload_config = config.security.upload.clone();
    let max_request_size = upload_config.max_request_size_bytes;
    tracing::info!(
        max_request_size_bytes = max_request_size,
        max_file_size_bytes = upload_config.max_file_size_bytes,
        allowed_mime_types = ?upload_config.allowed_mime_types,
        "Request body size limits enabled (applies to all content types)"
    );

    // Apply a global body-size cap covering JSON, form, raw bytes, and multipart.
    // The Multipart extractor further refines this per the UploadConfig extension.
    let router = router.layer(axum::extract::DefaultBodyLimit::max(max_request_size));

    // Insert UploadConfig into extensions so the Multipart extractor can read
    // per-file limits and the allowed MIME-type list.
    router.layer(axum::middleware::from_fn(
        move |mut req: axum::extract::Request, next: axum::middleware::Next| {
            let upload_config = upload_config.clone();
            async move {
                req.extensions_mut().insert(upload_config);
                next.run(req).await
            }
        },
    ))
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
    let bypass_paths = vec![
        config.health.path.clone(),
        config.health.live_path.clone(),
        config.health.ready_path.clone(),
        config.health.startup_path.clone(),
        crate::actuator::actuator_route_path(&config.actuator.prefix, "/health"),
    ];
    crate::middleware::maintenance::MaintenanceLayer::new(maintenance_state)
        .with_health_prefix(config.actuator.prefix.clone())
        .with_probe_paths(bypass_paths)
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
fn build_route_timeout_table(
    route_list: &[Route],
    scoped_groups: &[ScopedGroup],
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
        // Key by (path, *effective request method*) so an override on one handler
        // never bleeds onto sibling methods that share the template, while still
        // resolving when the request reaches the handler through a method alias.
        // `request_timeout_handler` looks up `req.method()`, which differs from
        // the declared method in two cases:
        //   - axum serves `HEAD` through a `#[get]` handler, so a GET override
        //     must also cover HEAD.
        //   - `#[ws]` records the synthetic `WS` method but mounts a `GET`
        //     handler, so the upgrade (and its auth work) arrives as GET.
        // Each (effective method, path) pair is still unique across the router, so
        // `insert` cannot lose a competing entry.
        let by_method = table.entry(path).or_default();
        match method.as_str() {
            "WS" => {
                by_method.insert(http::Method::GET, timeout);
            }
            _ if *method == http::Method::GET => {
                by_method.insert(http::Method::GET, timeout);
                by_method.insert(http::Method::HEAD, timeout);
            }
            _ => {
                by_method.insert(method.clone(), timeout);
            }
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
    std::sync::Arc::new(table)
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
fn apply_request_timeout_middleware(
    router: axum::Router<AppState>,
    config: &AutumnConfig,
    metrics: crate::middleware::MetricsCollector,
    route_timeouts: RouteTimeoutTable,
    mirror_cors: bool,
) -> axum::Router<AppState> {
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
        return router;
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
    router.layer(axum::middleware::from_fn(move |req, next| {
        request_timeout_handler(
            req,
            next,
            global,
            route_timeouts.clone(),
            metrics.clone(),
            cors.clone(),
        )
    }))
}

async fn request_timeout_handler(
    req: axum::extract::Request,
    next: axum::middleware::Next,
    global: Option<std::time::Duration>,
    route_timeouts: RouteTimeoutTable,
    metrics: crate::middleware::MetricsCollector,
    cors: Option<std::sync::Arc<crate::config::CorsConfig>>,
) -> axum::response::Response {
    // Internal `autumn build` / ISR regeneration renders drive a `#[static_get]`
    // route directly via `oneshot` and tag the request with `RenderDeadlineExempt`
    // (there is no client connection whose deadline should apply). Skip the
    // deadline for these; live inbound requests to the same route do not carry
    // the marker and are bounded normally below.
    if req
        .extensions()
        .get::<crate::static_gen::RenderDeadlineExempt>()
        .is_some()
    {
        return next.run(req).await;
    }

    // Resolve the effective deadline from the matched route template + method,
    // using borrowed lookups so exempt/disabled routes allocate nothing.
    let matched_path_ref = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(axum::extract::MatchedPath::as_str);
    let route_timeout = matched_path_ref
        .and_then(|p| route_timeouts.get(p))
        .and_then(|by_method| by_method.get(req.method()))
        .copied()
        .unwrap_or(crate::route::RouteTimeout::Inherit);
    let deadline = match route_timeout {
        crate::route::RouteTimeout::Disabled => None,
        crate::route::RouteTimeout::Override(d) => Some(d),
        crate::route::RouteTimeout::Inherit => global,
    };
    let Some(duration) = deadline else {
        // Exempt (disabled route, or global off with a non-Override route) —
        // no allocation on this hot path.
        return next.run(req).await;
    };

    // A deadline is active: now it's worth owning the path for the warn log.
    let matched_path = matched_path_ref.map(ToOwned::to_owned);
    let request_id = req
        .extensions()
        .get::<crate::middleware::RequestId>()
        .cloned();
    // Capture the request Origin before `req` is consumed so a timeout 503 can
    // mirror the CORS headers `CorsLayer` would have added (only when mirroring
    // is enabled — see `apply_request_timeout_middleware`).
    let cors_origin = cors
        .as_ref()
        .and_then(|_| req.headers().get(http::header::ORIGIN).cloned());
    let start = std::time::Instant::now();
    match tokio::time::timeout(duration, next.run(req)).await {
        Ok(response) => response,
        Err(_elapsed) => {
            let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            let route = matched_path.as_deref().unwrap_or("<unmatched>");
            // Structured telemetry: route template + elapsed time so operators
            // can alert on the (already-counted) timeout event.
            tracing::warn!(
                target: "autumn::timeout",
                route = route,
                elapsed_ms = elapsed_ms,
                timeout_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                request_id = request_id.as_ref().map(ToString::to_string),
                "inbound request exceeded deadline"
            );
            metrics.record_request_timeout();
            // Return a 503 via the standard error type so the exception-filter
            // and error-page stack negotiate JSON vs HTML and enrich with the
            // request id — no manual Problem Details assembly, no raw BoxError.
            let mut response =
                crate::error::AutumnError::service_unavailable(RequestDeadlineExceeded {
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
            if let Some(cors) = cors.as_deref() {
                apply_cors_headers_to_timeout_response(cors, cors_origin.as_ref(), &mut response);
            }
            response
        }
    }
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
        .with_metrics(state.metrics.clone());

    Ok(Some(BuiltIdempotencyLayers {
        route: base.clone().replay_through_inner(),
        manual: base.fail_closed_on_replay(),
    }))
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

    router = apply_cors_middleware(router, config);
    let trusted_host_policy = TrustedHostPolicy::from_config(config);
    router = router.layer(axum::middleware::from_fn(move |req, next| {
        trusted_host_middleware(req, next, trusted_host_policy.clone())
    }));
    router = apply_csrf_middleware(router, config, signing_keys_opt.clone());
    router = apply_bot_protection_middleware(router, config);
    // Method-override rejection filter. The outer `MethodOverrideLayer`
    // (applied at the `axum::serve` boundary so it can rewrite the
    // request method before route matching) stamps a
    // [`MethodOverrideRejection`] extension when the override field
    // value is invalid or the body was too large to scan; this inner
    // middleware converts that extension into the corresponding
    // `400`/`413` response. Running it here means the rejection flows
    // through the rest of the response stack (security headers,
    // request IDs, metrics, error-page filter) rather than bypassing
    // them. Placed outside CSRF so a `BodyTooLarge` (empty body)
    // doesn't get masked by a `403` from CSRF's missing-token branch,
    // and a clear `400 invalid _method` outranks "missing CSRF".
    router = router.layer(axum::middleware::from_fn(
        crate::middleware::method_override_rejection_filter,
    ));
    router = apply_rate_limit_middleware(router, config, state);

    // Register MaintenanceLayer automatically (shared construction with the
    // late-mounted `/mcp` envelope — see `build_maintenance_layer`).
    router = router.layer(build_maintenance_layer(config, state));

    router = router.layer(axum::middleware::from_fn(
        crate::webhook::webhook_replay_cleanup_middleware,
    ));
    router = apply_upload_middleware(router, config);

    // User-registered Tower layers (AppBuilder::layer). Outermost — applied
    // last so they wrap all framework middleware.  Iterate in reverse so the
    // first registered layer ends up outermost among user layers — matching
    // tower::ServiceBuilder ordering.
    //
    // When a static dist dir is active (SSG/ISG build), these layers are
    // NOT passed here — they are extracted by try_build_router_with_static_inner
    // and applied outside the static-first middleware instead, so they can
    // process pre-rendered responses without creating a session dependency.
    let custom_layer_count = custom_layers.len();
    for registered in custom_layers.into_iter().rev() {
        router = (registered.apply)(router);
    }
    if custom_layer_count > 0 {
        tracing::debug!(count = custom_layer_count, "Custom Tower layers applied");
    }

    // TrustedProxiesLayer is applied after user layers so it is outermost in the
    // ingress request path, stamping ResolvedClientIdentity before any user or
    // framework middleware reads ClientAddr / ClientHost / ClientScheme.
    router = apply_trusted_proxies_middleware(router, config);

    let mut router = router;

    if config.tenancy.enabled {
        router = router.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::tenancy::tenancy_middleware,
        ));
        tracing::debug!("Multi-tenancy middleware enabled");
    }

    // Per-request timeout (inner to RequestId so the request ID set by that
    // layer is available when the timeout fires — see request_timeout_handler).
    //
    // Full ingress layer order (outermost → innermost):
    //   TraceContext → AccessLog-fallback (applied in apply_startup_barrier) →
    //   StartupBarrier → Compression → Metrics → ExceptionFilter → ErrorPageContext →
    //   Session → SecurityHeaders → RequestId → LogContext → AccessLog-primary →
    //   Timeout → [user layers] → Tenancy → BodyLimit/UploadConfig →
    //   MethodOverride → RateLimit → CSRF → CORS → handler
    // `mirror_cors = true`: this layer is outside `CorsLayer` (CORS is applied
    // earlier, hence inner), so its timeout 503 must carry CORS headers itself.
    //
    // KNOWN LIMITATION (session store I/O is not bounded): `Session` sits outside
    // this layer (see order above), so `store.load` runs before the timer starts
    // and `store.save`/`destroy` after it completes. A stalled session backend can
    // therefore tie up a worker despite `request_timeout_ms`. This placement is
    // deliberate: the timer is kept inner to `RequestId` so a timeout 503 (and its
    // warn log) carries `X-Request-Id` for log correlation — moving it outside
    // `Session` would also move it outside `RequestId` and lose that. Operators
    // who need to bound session-store I/O should configure a store-level deadline
    // (e.g. the Redis command/connection timeout); a cancelled inbound request
    // cannot abort an already-issued store call regardless of layer order.
    //
    // The same applies to the edge layers `App::run` wraps around the finished
    // router at the `axum::serve` boundary (`MethodOverrideLayer`,
    // `TrustedProxiesLayer`): they sit outside `RequestId` and therefore outside
    // this timer. In particular `MethodOverrideLayer` buffers an HTML form body
    // (`axum::body::to_bytes`, capped at `upload.max_request_size_bytes`) before
    // the inner router runs, so a slow `_method` form upload is not bounded by
    // `request_timeout_ms`. Moving the timer out there would again lose the
    // `X-Request-Id` correlation; bound this with a server/proxy read timeout
    // instead.
    router = apply_request_timeout_middleware(
        router,
        config,
        state.metrics.clone(),
        route_timeouts,
        true,
    );

    // Error-reporting + panic-catch layer. Placed inner to `RequestIdLayer`
    // (so the request id is available when a handler panics) and outer to the
    // timeout, user layers, and handler (so their panics are caught and turned
    // into a clean 500 instead of aborting the worker task). The resulting 500
    // still flows out through the exception-filter chain for HTML negotiation.
    #[cfg(feature = "reporting")]
    {
        router = router.layer(crate::reporting::ReportingLayer::new(
            state.error_reporters(),
            config.reporting.enabled,
            config.reporting.sample_rate,
        ));
    }

    // Structured per-request access log (#999), primary emitter: one INFO
    // event (target `autumn::access`) per served request at the response
    // boundary. Inner to RequestId (so the request id is available) and to
    // LogContext (so the event is emitted inside the request span); outer to
    // the reporting and timeout layers so panics-turned-500s and timeout
    // responses are logged with the status the client receives. Emitted
    // responses are marked so the outermost fallback (apply_startup_barrier)
    // does not double-log; that fallback covers requests that short-circuit
    // before this layer runs.
    if config.log.access_log {
        router = router.layer(crate::middleware::AccessLogLayer::new(
            config.log.access_log_exclude.clone(),
        ));
    }

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
    let router = router.layer(crate::middleware::LogContextLayer::new(log_context_filter));

    // `security_headers` is applied LATER as the framework's outermost layer
    // (after the gate, below) so that a gate short-circuit (redirect/401) still
    // carries HSTS/CSP/nosniff — see the application point after the gate loop.
    // RequestId stays here (inner to session) so the request id seeds the
    // session, logs, and trace context.
    let router = router.layer(RequestIdLayer);

    // Pre-clone signing keys for the RYWW middleware (session mode needs to
    // sign/verify the `autumn.ryw` cookie; `signing_keys_opt` is consumed below).
    #[cfg(feature = "db")]
    let signing_keys_for_ryw = signing_keys_opt.clone();

    let router = crate::session::apply_session_layer(
        router,
        &config.session,
        config.profile.as_deref(),
        session_store,
        signing_keys_opt,
    )?;
    tracing::debug!(backend = ?config.session.backend, "Session management enabled");

    // Read-your-own-writes middleware: installed only when the mode is not
    // `off`. When active, it scopes a per-request task-local `RequestPin`
    // that generated repository read methods consult at acquire time.
    // Inner to Session so the task-local wraps the handler; the `autumn.ryw`
    // cookie is parsed from raw `Cookie` headers and does not require the
    // Session extractor to have run first.
    #[cfg(feature = "db")]
    let router = if config.database.read_your_writes == crate::config::ReadYourWrites::Off {
        router
    } else {
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
        router.layer(axum::middleware::from_fn(move |req, next| {
            crate::read_your_writes::middleware(
                req,
                next,
                ryw_mode,
                window_secs,
                keys.clone(),
                metrics.clone(),
            )
        }))
    };

    // Error page filter: renders HTML error pages for browser requests.
    // Always registered (uses default renderer if no custom one is provided).
    let is_dev = config
        .profile
        .as_deref()
        .map_or(cfg!(debug_assertions), |p| p == "dev");

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

    // Error page context layer must be inner to the exception filter so
    // WantsHtml is set on the response before the filter inspects it.
    // Full ingress layer order (outermost -> innermost). NOTE: the framework's
    // outermost `SecurityHeadersLayer` and the `static_gate` layers are applied
    // by `build_router_pre_state` AFTER this function returns (and, crucially,
    // after the MCP dispatch clone is taken), so they are NOT in this list:
    //   SecurityHeaders (framework outermost — applied in build_router_pre_state) ->
    //   [static_gate layers — applied in build_router_pre_state, after the MCP
    //   dispatch clone, outside session and the static cache] ->
    //   TraceContext (applied outside the startup barrier so short-circuit
    //   responses still carry traceparent) ->
    //   Compression (outer to ExceptionFilter — see note below) ->
    //   [user layers, when SSG/ISG dist dir active] ->
    //   StaticFileMiddleware (when SSG/ISG enabled) ->
    //   Metrics -> ExceptionFilter -> ErrorPageContext -> Session ->
    //   RequestId -> LogContext -> AccessLog-primary ->
    //   [user layers, non-static build] ->
    //   Tenancy -> RateLimit -> CSRF -> CORS -> handler
    //   (An AccessLog fallback sits outermost, applied in apply_startup_barrier.)
    let router = router
        .layer(crate::middleware::error_page_filter::ErrorPageContextLayer { is_dev })
        .layer(ExceptionFilterLayer::new(all_filters))
        .layer(crate::middleware::MetricsLayer::new(state.metrics.clone()));

    // Response compression is applied outermost (outside ExceptionFilter) so that
    // exception filters which rebuild the response body (e.g. ProblemDetailsFilter
    // normalising AutumnErrors to JSON Problem Details) do so before the body is
    // encoded. If compression were inner to ExceptionFilter, the filter would
    // inherit a Content-Encoding: gzip header on the rebuilt uncompressed body,
    // causing clients to receive uncompressed bytes labeled as gzip.
    // User-registered layers (EtagLayer etc.) remain inner to Compression, so
    // ETags are still computed on the uncompressed body before encoding occurs.
    let router = apply_compression_middleware(router, config);

    // NOTE: the `static_gate` layers and the framework's outermost
    // `SecurityHeadersLayer` are intentionally NOT applied here. They are applied
    // by `build_router_pre_state` after this function returns and after the MCP
    // dispatch clone is taken, so a `tools/call` replay never traverses the
    // page-cache gate (matching the SSG/ISG path and the documented intent).
    Ok(router)
}

/// Apply a set of user-registered layer registrations so that the
/// first-registered layer ends up outermost on ingress — matching
/// [`tower::ServiceBuilder`] ordering. Returns the wrapped router.
fn apply_layers_in_registration_order(
    mut router: axum::Router<AppState>,
    layers: Vec<crate::app::CustomLayerRegistration>,
    what: &str,
) -> axum::Router<AppState> {
    let count = layers.len();
    for registered in layers.into_iter().rev() {
        router = (registered.apply)(router);
    }
    if count > 0 {
        tracing::debug!(count, "{what} Tower layers applied");
    }
    router
}

async fn trusted_host_middleware(
    req: Request<axum::body::Body>,
    next: Next,
    policy: TrustedHostPolicy,
) -> axum::response::Response {
    let path = req.uri().path();
    if (req.method() == http::Method::GET || req.method() == http::Method::HEAD)
        && policy.probe_bypass_paths.contains(path)
    {
        return next.run(req).await;
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
        return next.run(req).await;
    }
    if host.as_deref().is_some_and(|host| policy.allows_host(host)) {
        next.run(req).await
    } else {
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
        (
            StatusCode::BAD_REQUEST,
            [(http::header::CONTENT_TYPE, "application/problem+json")],
            body,
        )
            .into_response()
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
    // OUTSIDE the static-first middleware (and outside session) so that:
    //   • User layers (e.g. compression) can process pre-rendered responses.
    //   • Static serving remains available even if the session backend is down.
    //   • ISR regeneration uses the inner router (no user layers), ensuring
    //     re-rendered pages are saved as raw HTML rather than pre-transformed.
    //
    // KNOWN LIMITATION (`request_timeout_ms` does not bound these outer layers):
    // the per-request timeout lives inside `inner_router` (applied by
    // `apply_middleware`, inner to `RequestId`). Because `custom_layers` and
    // `static_gate_layers` are reapplied OUTSIDE the static-first middleware
    // (below), they — and the static cache lookup itself — run before the timer
    // starts. So when a `dist` manifest is active, a hung async `static_gate`
    // (e.g. remote JWT/IdP validation) or custom layer is NOT bounded by
    // `request_timeout_ms`, unlike the non-static path where the timer wraps the
    // user layers and tenancy. This is the same trade-off as the documented
    // session-store and edge-layer (`MethodOverrideLayer`, `TrustedProxiesLayer`)
    // limitations in `apply_middleware`: pulling the timer out here to cover them
    // would place it outside `RequestId` (losing `X-Request-Id` on the timeout
    // 503), double-time dynamic misses, and apply a global deadline to cached
    // hits that have no route-table entry. Operators who terminate auth/tenant
    // work in a `static_gate` should bound it with a layer-level or
    // server/proxy read timeout instead.
    //
    // Compute the idempotency flag NOW while custom_layers is still populated,
    // then drain it. build_router_pre_state would otherwise see an empty list
    // and incorrectly treat opaque layers as absent when selecting idempotency
    // behaviour for each route.
    //
    // Pre-static gate layers count here too: a `static_gate` used as a
    // JWT/stateless auth layer is an opaque app layer for idempotency purposes
    // (idempotency keys exclude `Authorization`, so without fail-closed replay a
    // second principal with the same key+body could receive the first
    // principal's cached mutation). Include them BEFORE either list is drained.
    let opaque_present = Some(
        custom_layers_require_fail_closed_idempotency(&ctx.custom_layers)
            || custom_layers_require_fail_closed_idempotency(&ctx.static_gate_layers),
    );
    let custom_layers = std::mem::take(&mut ctx.custom_layers);

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
        // regeneration drives this router directly and never reaches that outer
        // layer. `SecurityHeadersLayer` is also what injects `CspNonce` into
        // request extensions, so without it a handler using the `CspNonce`
        // extractor would 500 during regeneration and the stale file would never
        // refresh. Re-attach the layer here, on the regeneration router only.
        // Its response headers are discarded (only the rendered HTML body is
        // persisted), so this does not affect live-request headers and avoids the
        // duplicate-header / nonce conflict that a second live layer would cause.
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

    // Static-first serving: intercept GET/HEAD requests whose path appears
    // in the manifest and serve pre-built HTML directly — BEFORE the dynamic
    // router (and session layer) runs. This preserves availability of static
    // pages even when the session backend is unavailable.
    //
    // Requests not in the manifest (including non-GET/HEAD methods) fall
    // through to the dynamic router unchanged.
    //
    // ISR staleness checking happens inside `resolve()`: stale pages are
    // still served immediately while background regeneration runs
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
                    if let Some(file_path) = static_layer.resolve(normalized)
                        && let Ok(contents) = tokio::fs::read(&file_path).await
                    {
                        let body = if is_head {
                            axum::body::Body::empty()
                        } else {
                            axum::body::Body::from(contents)
                        };
                        return http::Response::builder()
                            .status(http::StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, "text/html; charset=utf-8")
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
    // the way out). Iterate in reverse so the first registered layer ends up
    // outermost — matching tower::ServiceBuilder ordering.
    router = apply_layers_in_registration_order(
        router,
        custom_layers,
        "Custom (outside static middleware)",
    );

    // Compression must also be applied OUTSIDE the static-first middleware so
    // that pre-rendered HTML pages (served directly by StaticFileLayer without
    // reaching inner_router) are also compressed. This mirrors the placement in
    // apply_middleware for the dynamic-only path.
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
struct StartupBarrierState {
    app_state: AppState,
    live_path: String,
    ready_path: String,
    startup_path: String,
    health_path: String,
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
            live_path: config.health.live_path.clone(),
            ready_path: config.health.ready_path.clone(),
            startup_path: config.health.startup_path.clone(),
            health_path: config.health.path.clone(),
            actuator_paths: crate::actuator::actuator_endpoint_paths(
                &config.actuator.prefix,
                config.actuator.sensitive,
                config.actuator.prometheus,
            ),
            actuator_subtree_paths,
        }
    }

    fn allows_path(&self, path: &str) -> bool {
        path == self.live_path
            || path == self.ready_path
            || path == self.startup_path
            || path == self.health_path
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
    let router = router.layer(axum::middleware::from_fn_with_state(
        barrier_state,
        startup_barrier,
    ));
    // Access-log fallback (#999), applied OUTSIDE the startup barrier, the
    // static-first (SSG/ISR) middleware, the session layer, and the
    // exception-filter chain — every production build path funnels through
    // this function, including after the late MCP endpoint merge. It emits
    // only for responses the primary in-stack layer never saw (it checks the
    // AccessLogEmitted response marker), giving startup 503s, pre-built
    // static page hits, session-store outage 503s, and MCP endpoint requests
    // an access line too. Those short-circuits never ran RequestIdLayer, so
    // the fallback reads `x-request-id` from the response when present and
    // logs without a request id otherwise.
    let router = if config.log.access_log {
        router.layer(crate::middleware::AccessLogLayer::fallback(
            config.log.access_log_exclude.clone(),
        ))
    } else {
        router
    };
    // W3C Trace Context propagation wraps the startup barrier (and the
    // static-first middleware above it) so short-circuit responses —
    // startup 503s and pre-built static file hits — still extract the
    // incoming `traceparent` and inject the current context into the
    // outgoing response. Applied here rather than inside `apply_middleware`
    // because those outer wrappers can return without ever invoking the
    // inner router. Outer to AccessLog so the access event is emitted while
    // the trace context is current.
    #[cfg(feature = "telemetry-otlp")]
    let router = router.layer(crate::middleware::TraceContextLayer);
    router
}

async fn startup_barrier(
    State(state): State<StartupBarrierState>,
    request: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    if crate::app::is_static_build_mode()
        || state.app_state.probes().is_startup_complete()
        || state.allows_path(request.uri().path())
    {
        next.run(request).await
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Service is still starting up",
        )
            .into_response()
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
fn apply_cors_headers_to_timeout_response(
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

/// Serves the framework's default flash-message stylesheet
/// ([`crate::flash::FLASH_CSS`]) at [`crate::flash::FLASH_CSS_PATH`].
#[cfg(feature = "flash")]
pub async fn flash_css_handler() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [
            (http::header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (
                http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ),
        ],
        crate::flash::FLASH_CSS,
    )
        .into_response()
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

/// Serves the vendored idiomorph DOM-morphing library at [`crate::htmx::IDIOMORPH_JS_PATH`].
///
/// Idiomorph enables smooth DOM morphing via `hx-swap="morph"` in htmx.
#[cfg(feature = "htmx")]
pub async fn idiomorph_handler() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [
            (http::header::CONTENT_TYPE, "application/javascript"),
            (
                http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ),
        ],
        crate::htmx::IDIOMORPH_JS,
    )
        .into_response()
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
fn collect_openapi_docs(
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

/// Scope the request's [`AppState`] as the ambient event-bus app for the
/// duration of the request, so the free `events::publish` resolves this app.
async fn event_app_context_middleware(
    state: axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    crate::events::scope_event_app(state.0.clone(), async move { next.run(req).await }).await
}

#[cfg(feature = "oauth2")]
async fn http_interceptor_middleware(
    state: axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use crate::interceptor::{ACTIVE_HTTP_INTERCEPTORS, HttpInterceptor};
    if let Some(interceptor_arc) = state.extension::<Arc<dyn HttpInterceptor>>() {
        let interceptor = (*interceptor_arc).clone();
        let interceptors = vec![interceptor];
        ACTIVE_HTTP_INTERCEPTORS
            .scope(interceptors, async move { next.run(req).await })
            .await
    } else {
        next.run(req).await
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
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: Some("test".to_owned()),
            started_at: std::time::Instant::now(),
            health_detailed: false,
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
        }
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
            let response = rt.block_on(async {
                app.oneshot(
                    Request::builder()
                        .uri("/not-a-probe")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            });
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
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

    #[cfg(feature = "mail")]
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

    #[cfg(feature = "openapi")]
    #[test]
    fn extract_path_params_matches_macro_behavior() {
        assert_eq!(
            super::extract_path_params("/orgs/{org_id}/users/{id}"),
            vec!["org_id".to_owned(), "id".to_owned()]
        );
        assert!(super::extract_path_params("/static").is_empty());
        assert_eq!(
            super::extract_path_params("/users/{id:[0-9]+}"),
            vec!["id".to_owned()]
        );
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
            api_version: None,
            sunset_opt_out: false,
        };

        let ctx = RouterContext {
            exception_filters: Vec::new(),
            scoped_groups: Vec::new(),
            merge_routers: Vec::new(),
            nest_routers: Vec::new(),
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
            api_version: None,
            sunset_opt_out: false,
        };

        let ctx = RouterContext {
            exception_filters: Vec::new(),
            scoped_groups: Vec::new(),
            merge_routers: Vec::new(),
            nest_routers: Vec::new(),
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

    // --- Static file serving (SSG/ISG) tests ---

    fn create_static_dist(revalidate: Option<u64>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let dist = dir.path().join("dist");
        std::fs::create_dir_all(dist.join("about")).expect("mkdir about");
        std::fs::write(dist.join("index.html"), b"<h1>Home</h1>").expect("write index");
        std::fs::write(dist.join("about/index.html"), b"<h1>About</h1>").expect("write about");

        let mut routes = std::collections::HashMap::new();
        routes.insert(
            "/".to_owned(),
            crate::static_gen::ManifestEntry {
                file: "index.html".to_owned(),
                revalidate: None,
            },
        );
        routes.insert(
            "/about".to_owned(),
            crate::static_gen::ManifestEntry {
                file: "about/index.html".to_owned(),
                revalidate,
            },
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
        .layer(RequestIdLayer)
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
        .layer(RequestIdLayer)
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
        .layer(RequestIdLayer)
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
        .layer(RequestIdLayer)
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
        .layer(RequestIdLayer)
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
        .layer(RequestIdLayer)
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

        // No RequestIdLayer — exercises the else branch in request_timeout_handler.
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
        let table = build_route_timeout_table(&[], &[]);
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

        let table = build_route_timeout_table(&routes, &[]);

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

        let table = build_route_timeout_table(&[], &[make_group("/api/")]);
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
        let table = build_route_timeout_table(&[], &[make_group("/api")]);
        assert_eq!(
            table.get("/api").and_then(|m| m.get(&http::Method::GET)),
            Some(&override_5s),
        );
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
            apply: Box::new(move |router| router.layer(gate)),
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
            crate::static_gen::ManifestEntry {
                file: "index.html".to_owned(),
                revalidate: None,
            },
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
        let probe_bypass_paths = std::collections::HashSet::from([
            config.health.path.clone(),
            config.health.live_path.clone(),
            config.health.ready_path.clone(),
            config.health.startup_path.clone(),
            crate::actuator::actuator_route_path(&config.actuator.prefix, "/health"),
        ]);
        Self {
            rules: Arc::new(rules),
            allow_any,
            allow_missing_host: !is_production,
            probe_bypass_paths: Arc::new(probe_bypass_paths),
        }
    }

    /// Whether a request carrying no usable `Host` is allowed through. Mirrors
    /// `trusted_host_middleware`'s missing-host branch for callers (e.g. the MCP
    /// envelope) that enforce the policy outside that middleware.
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
        assert!(
            cc.contains("immutable"),
            "expected immutable cache-control, got: {cc}"
        );

        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!body.is_empty(), "idiomorph JS body must be non-empty");
    }
}
