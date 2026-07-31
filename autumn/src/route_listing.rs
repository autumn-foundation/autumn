//! Route listing types and collection logic for `autumn routes`.
//!
//! Collects route metadata from an [`AppBuilder`](crate::app::AppBuilder) into
//! serializable [`RouteInfo`] values that the CLI can consume without booting
//! the full HTTP server.

use serde::{Deserialize, Serialize};

use crate::app::ScopedGroup;
use crate::route::Route;

/// Machine-readable stderr marker for omitted (unenumerable) raw routers.
///
/// Emitted by the `AUTUMN_DUMP_ROUTES` dump when raw routers registered via
/// [`AppBuilder::merge`](crate::app::AppBuilder::merge) /
/// [`AppBuilder::nest`](crate::app::AppBuilder::nest) are omitted from the JSON
/// route listing (they are opaque and cannot be enumerated). The decimal count
/// of omitted routers follows the marker on the same line.
///
/// This is a process-boundary protocol: `autumn routes audit` runs the built
/// binary as a child, scans its stderr for this marker, and hard-fails the
/// coverage gate when the count is non-zero — omitted routes can carry unproven
/// auth posture, so a manifest that silently drops them would defeat the gate.
pub const OMITTED_ROUTES_MARKER: &str = "[autumn:omitted-routes] ";

/// Machine-readable stderr marker carrying the resolved security configuration
/// (CSRF + headers) for the `declared` manifest dimensions built by
/// `autumn routes audit`.
///
/// Emitted by the `AUTUMN_DUMP_ROUTES` dump only when `AUTUMN_DUMP_SECURITY=1`
/// is *also* set — which `autumn routes audit` sets, but the plain
/// `autumn routes` listing does not. A single compact JSON [`SecurityDump`]
/// follows the marker on the same line. Kept off stdout so the routes-only JSON
/// parse path (`autumn routes`, and older audit builds) stays byte-compatible.
pub const SECURITY_CONFIG_MARKER: &str = "[autumn:security-config] ";

/// Resolved CSRF configuration carried across the dump boundary for the
/// `declared` CSRF manifest dimension.
///
/// Mirrors the runtime-relevant subset of
/// [`CsrfConfig`](crate::security::config::CsrfConfig): the enable flag plus the
/// two inputs to the runtime safe/exempt predicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CsrfDump {
    /// Whether CSRF enforcement is enabled.
    pub enabled: bool,
    /// Methods that never require a CSRF token (sorted).
    pub safe_methods: Vec<String>,
    /// Path prefixes exempt from CSRF validation (sorted).
    pub exempt_paths: Vec<String>,
}

/// Resolved security-headers configuration carried across the dump boundary for
/// the `declared` security-headers manifest dimension.
///
/// Every string is the *effective* declared value — in particular
/// `content_security_policy` is the resolved template
/// ([`default_content_security_policy`](crate::security::config::default_content_security_policy)
/// when unset), not an unresolved sentinel. When per-request CSP nonce injection
/// is active it is the nonce-aware template (with a stable `AUTUMN_CSP_NONCE`
/// placeholder), matching what responses actually send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct HeadersDump {
    /// `X-Frame-Options` value (empty = header not emitted).
    pub x_frame_options: String,
    /// Whether `X-Content-Type-Options: nosniff` is sent.
    pub x_content_type_options: bool,
    /// Whether `X-XSS-Protection: 1; mode=block` is sent.
    pub xss_protection: bool,
    /// Resolved `Content-Security-Policy` value (empty = header not emitted).
    pub content_security_policy: String,
    /// `Referrer-Policy` value (empty = header not emitted).
    pub referrer_policy: String,
    /// `Permissions-Policy` value (empty = header not emitted).
    pub permissions_policy: String,
    /// Whether `Strict-Transport-Security` (HSTS) is sent.
    pub strict_transport_security: bool,
    /// HSTS `max-age` in seconds.
    pub hsts_max_age_secs: u64,
    /// Whether HSTS includes subdomains.
    pub hsts_include_subdomains: bool,
    /// Whether per-request CSP nonce injection is enabled.
    pub csp_nonce: bool,
}

/// Resolved security configuration snapshot emitted after
/// [`SECURITY_CONFIG_MARKER`] for the manifest's `declared` dimensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityDump {
    /// CSRF configuration.
    pub csrf: CsrfDump,
    /// Security-headers configuration.
    pub headers: HeadersDump,
}

impl SecurityDump {
    /// Snapshot the security-relevant configuration for the manifest dump.
    ///
    /// Every emitted list is sorted so the serialized snapshot is
    /// byte-deterministic across runs, regardless of config source ordering.
    #[must_use]
    pub fn from_config(config: &crate::config::AutumnConfig) -> Self {
        let csrf = &config.security.csrf;
        let headers = &config.security.headers;

        let mut safe_methods = csrf.safe_methods.clone();
        safe_methods.sort();
        safe_methods.dedup();
        let mut exempt_paths = csrf.exempt_paths.clone();
        // Runtime's `apply_csrf_middleware` exempts every configured webhook
        // endpoint path via `CsrfLayer::with_exempt_path`, so a webhook POST
        // route not also listed in `csrf.exempt_paths` still skips CSRF at
        // runtime. Fold those paths into the exempt set so the manifest matches.
        for endpoint in &config.security.webhooks.endpoints {
            exempt_paths.push(endpoint.path.clone());
        }
        // Runtime also exempts the framework-owned RFC 8058 one-click unsubscribe
        // endpoint from CSRF (`apply_csrf_middleware`, `router.rs`) under the same
        // `mail`-feature gate and `should_mount_unsubscribe_endpoint()` predicate,
        // so fold that path in too when it applies.
        #[cfg(feature = "mail")]
        if config.mail.should_mount_unsubscribe_endpoint() {
            exempt_paths.push(crate::mail::UNSUBSCRIBE_PATH.to_owned());
        }
        exempt_paths.sort();
        exempt_paths.dedup();

        Self {
            csrf: CsrfDump {
                enabled: csrf.enabled,
                safe_methods,
                exempt_paths,
            },
            headers: HeadersDump {
                x_frame_options: headers.x_frame_options.clone(),
                x_content_type_options: headers.x_content_type_options,
                xss_protection: headers.xss_protection,
                content_security_policy: crate::security::headers::resolved_content_security_policy(
                    headers,
                ),
                referrer_policy: headers.referrer_policy.clone(),
                permissions_policy: headers.permissions_policy.clone(),
                strict_transport_security: headers.strict_transport_security,
                hsts_max_age_secs: headers.hsts_max_age_secs,
                hsts_include_subdomains: headers.hsts_include_subdomains,
                csp_nonce: headers.csp_nonce.enabled,
            },
        }
    }
}

/// Where a route was registered: by the user application, by a named plugin,
/// or by the framework itself (probes, actuator, htmx assets, dev reload).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RouteSource {
    /// Registered directly by the user application.
    #[default]
    User,
    /// Registered by a named autumn plugin (e.g. `"admin"` for autumn-admin-plugin).
    Plugin(String),
    /// Registered by the framework (probes, actuator, htmx assets, dev reload).
    Framework,
}

/// Security posture of a route.
///
/// Derived at build time from the handler's macro-expanded
/// [`ApiDoc`](crate::openapi::ApiDoc) and its [`RouteSource`]. Emitted alongside
/// every route in the `autumn routes audit` manifest so route authentication
/// coverage can be gated in CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RouteClassification {
    /// Owned by the framework (probes, actuator, htmx assets, docs). Exempt
    /// from the audit gate — these are pre-attributed and never require an
    /// explicit `#[secured]`/`#[public]` declaration.
    Framework,
    /// Guarded by authentication (`#[secured]`) and/or dynamic policy
    /// authorization (`#[authorize]`).
    Gated,
    /// Explicitly declared unauthenticated via `#[public]`.
    Public,
    /// No security posture could be proven for this route. This is the state
    /// the audit gate fails on.
    #[default]
    Unclassified,
}

impl RouteClassification {
    /// Stable lowercase tag used in serialized manifests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Framework => "framework",
            Self::Gated => "gated",
            Self::Public => "public",
            Self::Unclassified => "unclassified",
        }
    }
}

impl std::fmt::Display for RouteClassification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for RouteClassification {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RouteClassification {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "framework" => Self::Framework,
            "gated" => Self::Gated,
            "public" => Self::Public,
            _ => Self::Unclassified,
        })
    }
}

impl std::fmt::Display for RouteSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User => write!(f, "user"),
            Self::Plugin(name) => write!(f, "plugin:{name}"),
            Self::Framework => write!(f, "framework"),
        }
    }
}

impl Serialize for RouteSource {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for RouteSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(if s == "framework" {
            Self::Framework
        } else if let Some(name) = s.strip_prefix("plugin:") {
            Self::Plugin(name.to_owned())
        } else {
            Self::User
        })
    }
}

/// Metadata for a single mounted route, suitable for display and JSON export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteInfo {
    /// HTTP method (`GET`, `POST`, `PUT`, `DELETE`, `PATCH`, `WS`, etc.).
    pub method: String,
    /// Full mounted URL path (e.g. `/api/posts/{id}`), reflecting the
    /// final URL after any scope prefix is applied.
    pub path: String,
    /// Handler function name (e.g. `"posts::show"`).
    pub handler: String,
    /// Registration origin: user application, a named plugin, or the framework.
    pub source: RouteSource,
    /// Active middleware on this route (compact labels, e.g. `"secured"`, `"cached(60s)"`).
    pub middleware: Vec<String>,
    /// API version of the route (e.g. "v1")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    /// Status of the version ("active", "deprecated", "sunset")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Whether this route opts out of sunset 410 Gone response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sunset_opt_out: Option<bool>,
    /// Build-time security classification derived from the handler's auth
    /// posture and registration source. Consumed by `autumn routes audit`.
    #[serde(default)]
    pub classification: RouteClassification,
    /// Roles required by `#[secured("role")]`, carried for `Gated` routes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Scopes required by `#[secured(scopes = [...])]`, carried for `Gated` routes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// Whether the route is guarded by dynamic policy authorization
    /// (`#[authorize]`), carried for `Gated` routes.
    #[serde(default, skip_serializing_if = "is_false")]
    pub policy: bool,
    /// Module path of the handler (from `module_path!()`), used to name a
    /// route in audit diagnostics. `None` for routes without a known module.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// `file:line` source location of the handler (from `file!()`/`line!()`),
    /// used to point an audit diagnostic straight at the offending handler.
    /// `None` for routes without a known location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// `skip_serializing_if` helper: elide `false` booleans from JSON output.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(b: &bool) -> bool {
    !*b
}

impl RouteInfo {
    /// Construct a framework-owned `GET` route entry. Framework routes are
    /// pre-classified and exempt from the audit gate.
    fn framework_get(path: String, handler: &str) -> Self {
        Self::framework_route("GET", path, handler)
    }

    /// Construct a framework-owned route entry for an arbitrary HTTP method.
    /// Framework routes are pre-classified and exempt from the audit gate.
    fn framework_route(method: &str, path: String, handler: &str) -> Self {
        Self {
            method: method.to_owned(),
            path,
            handler: handler.to_owned(),
            source: RouteSource::Framework,
            classification: RouteClassification::Framework,
            ..Self::default()
        }
    }
}

/// Derive a route's security classification (and carried posture) from its
/// registration source and macro-expanded [`ApiDoc`](crate::openapi::ApiDoc).
///
/// Precedence: framework routes are always [`RouteClassification::Framework`];
/// otherwise a route guarded by `#[secured]` or `#[authorize]` is
/// [`RouteClassification::Gated`] (carrying its roles/scopes/policy); an
/// explicit `#[public]` is [`RouteClassification::Public`]; anything left is
/// [`RouteClassification::Unclassified`].
///
/// Repository auto-API routes (`#[repository(api = ..., policy = ...)]`) carry
/// their record-level authorization guard on the route's
/// [`RepositoryApiMeta`](crate::route::RepositoryApiMeta), *not* on the
/// handler's [`ApiDoc`](crate::openapi::ApiDoc): the generated CRUD handlers
/// build their `ApiDoc` with `..Default::default()` and never set
/// `secured`/`has_policy`. So a `policy = ...` repository must be classified
/// from `repository.has_policy` — otherwise a policy-protected CRUD endpoint
/// would fail the audit gate as `Unclassified` despite enforcing authorization.
fn classify(
    source: &RouteSource,
    api_doc: &crate::openapi::ApiDoc,
    repository: Option<&crate::route::RepositoryApiMeta>,
) -> (RouteClassification, Vec<String>, Vec<String>, bool) {
    if matches!(source, RouteSource::Framework) {
        return (
            RouteClassification::Framework,
            Vec::new(),
            Vec::new(),
            false,
        );
    }
    let repo_has_policy = repository.is_some_and(|r| r.has_policy);
    // A repository auto-API declared with `scope = ...` (but no `policy = ...`)
    // enforces the registered scope on its generated list handler at runtime,
    // yet leaves both the handler `ApiDoc` and `RepositoryApiMeta::has_policy`
    // at their defaults. Treat the presence of a scope guard as gated too, so
    // these scope-protected `GET /<api>` routes are not false-failed as
    // `Unclassified`. No scope *name* is recorded on the meta (only a
    // type-erased registry probe), so there is nothing to carry into `scopes`.
    let repo_has_scope = repository.is_some_and(|r| r.scope_check.is_some());
    if api_doc.secured || api_doc.has_policy || repo_has_policy || repo_has_scope {
        let roles = api_doc
            .required_roles
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let scopes = api_doc
            .required_scopes
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        return (
            RouteClassification::Gated,
            roles,
            scopes,
            api_doc.has_policy || repo_has_policy,
        );
    }
    if api_doc.public {
        return (RouteClassification::Public, Vec::new(), Vec::new(), false);
    }
    (
        RouteClassification::Unclassified,
        Vec::new(),
        Vec::new(),
        false,
    )
}

/// The handler's module path, when the route macros captured one.
fn module_of(api_doc: &crate::openapi::ApiDoc) -> Option<String> {
    (!api_doc.module_path.is_empty()).then(|| api_doc.module_path.to_owned())
}

/// The handler's `file:line` source location, when the route macros captured
/// one (`ApiDoc::source_file`/`ApiDoc::source_line`, from `file!()`/`line!()`
/// at the handler's definition site). `None` for routes constructed without
/// the route macros, so an audit diagnostic can point straight at the
/// offending handler instead of only naming its module.
fn source_location_of(api_doc: &crate::openapi::ApiDoc) -> Option<String> {
    (!api_doc.source_file.is_empty())
        .then(|| format!("{}:{}", api_doc.source_file, api_doc.source_line))
}

/// Helper type alias representing version name, status string, and sunset opt-out flag.
type RouteVersionInfo = (Option<String>, Option<String>, Option<bool>);

/// Collect [`RouteInfo`] entries from user routes and scoped groups.
///
/// `route_sources` is a parallel slice to `routes`; each element is the
/// [`RouteSource`] for the corresponding route. If the slice is shorter than
/// `routes`, remaining routes are attributed to [`RouteSource::User`].
///
/// Does not include framework-internal routes (probes, actuator, htmx).
/// Call `append_framework_routes` with a loaded config to add those.
///
/// # Errors
///
/// Returns `RouterBuildError::UnregisteredApiVersion` if a route's version is not registered.
pub fn collect_route_infos(
    routes: &[Route],
    route_sources: &[RouteSource],
    scoped_groups: &[ScopedGroup],
    api_versions: &[crate::app::ApiVersion],
) -> Result<Vec<RouteInfo>, crate::router::RouterBuildError> {
    let mut infos = Vec::with_capacity(routes.len());
    let now = chrono::Utc::now();

    let resolve_status = |route_name: &str,
                          api_version: Option<&str>,
                          sunset_opt_out: bool|
     -> Result<RouteVersionInfo, crate::router::RouterBuildError> {
        let Some(ver) = api_version else {
            return Ok((None, None, None));
        };
        api_versions
            .iter()
            .find(|av| av.version == ver)
            .map_or_else(
                || {
                    Err(crate::router::RouterBuildError::UnregisteredApiVersion {
                        route_name: route_name.to_string(),
                        version: ver.to_string(),
                    })
                },
                |av| {
                    let is_sunset = av.sunset_at.is_some_and(|s| now >= s);
                    let is_dep = av.deprecated_at.is_some_and(|d| now >= d);
                    let status = if is_sunset {
                        "sunset"
                    } else if is_dep {
                        "deprecated"
                    } else {
                        "active"
                    };
                    Ok((
                        Some(ver.to_string()),
                        Some(status.to_string()),
                        Some(sunset_opt_out),
                    ))
                },
            )
    };

    for (i, route) in routes.iter().enumerate() {
        let source = route_sources.get(i).cloned().unwrap_or(RouteSource::User);
        let (api_version, status, sunset_opt_out) =
            resolve_status(route.name, route.api_version, route.sunset_opt_out)?;
        let (classification, roles, scopes, policy) =
            classify(&source, &route.api_doc, route.repository.as_ref());
        infos.push(RouteInfo {
            method: route.method.to_string(),
            path: route.path.to_owned(),
            handler: route.name.to_owned(),
            source,
            middleware: Vec::new(),
            api_version,
            status,
            sunset_opt_out,
            classification,
            roles,
            scopes,
            policy,
            module: module_of(&route.api_doc),
            location: source_location_of(&route.api_doc),
        });
    }

    for group in scoped_groups {
        for route in &group.routes {
            let full_path = join_scope_path(&group.prefix, route.path);
            let (api_version, status, sunset_opt_out) =
                resolve_status(route.name, route.api_version, route.sunset_opt_out)?;
            let (classification, roles, scopes, policy) =
                classify(&group.source, &route.api_doc, route.repository.as_ref());
            infos.push(RouteInfo {
                method: route.method.to_string(),
                path: full_path,
                handler: route.name.to_owned(),
                source: group.source.clone(),
                middleware: Vec::new(),
                api_version,
                status,
                sunset_opt_out,
                classification,
                roles,
                scopes,
                policy,
                module: module_of(&route.api_doc),
                location: source_location_of(&route.api_doc),
            });
        }
    }

    Ok(infos)
}

/// Append framework-internal routes (probes, actuator, htmx assets).
///
/// Paths are taken from `config` so custom probe/actuator prefix settings
/// are reflected accurately.
#[allow(clippy::too_many_lines)]
pub(crate) fn append_framework_routes(
    infos: &mut Vec<RouteInfo>,
    config: &crate::config::AutumnConfig,
) {
    let mut probe_paths = std::collections::HashSet::new();
    for (path, name) in [
        (config.health.live_path.as_str(), "live"),
        (config.health.ready_path.as_str(), "ready"),
        (config.health.startup_path.as_str(), "startup"),
        (config.health.path.as_str(), "health"),
    ] {
        if probe_paths.insert(path) {
            infos.push(RouteInfo::framework_get(path.to_owned(), name));
        }
    }

    // Mutating (non-GET) actuator routes are enumerated separately so they
    // carry their real HTTP method (e.g. `PUT /actuator/loggers/{name}`,
    // `POST /actuator/webhooks/replay`) rather than a phantom GET.
    let mutating_routes = crate::actuator::actuator_mutating_routes(
        &config.actuator.prefix,
        config.actuator.sensitive,
    );
    // Some entries in `actuator_endpoint_paths` exist only to seed the runtime
    // startup-barrier allow-list (`StartupBarrierState::from_config`) and are
    // actually mounted with a mutating method (e.g. `/webhooks/replay` is a
    // `POST`). Skip any path already covered by a mutating-method entry so the
    // GET-only listing does not emit a phantom GET for it.
    let mutating_paths: std::collections::HashSet<&str> = mutating_routes
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
        infos.push(RouteInfo::framework_get(path, "actuator"));
    }

    for (route_method, route_path) in &mutating_routes {
        infos.push(RouteInfo::framework_route(
            route_method,
            route_path.clone(),
            "actuator",
        ));
    }

    #[cfg(feature = "htmx")]
    {
        infos.push(RouteInfo::framework_get(
            crate::htmx::HTMX_JS_PATH.to_owned(),
            "htmx",
        ));
        infos.push(RouteInfo::framework_get(
            crate::htmx::HTMX_CSRF_JS_PATH.to_owned(),
            "htmx_csrf",
        ));
        infos.push(RouteInfo::framework_get(
            crate::htmx::IDIOMORPH_JS_PATH.to_owned(),
            "idiomorph",
        ));
        infos.push(RouteInfo::framework_get(
            crate::htmx::HTMX_SSE_JS_PATH.to_owned(),
            "htmx_sse",
        ));
    }

    #[cfg(feature = "mail")]
    if config
        .mail
        .preview_routes_enabled(config.profile.as_deref())
    {
        for (path, handler) in [
            (crate::mail::MAIL_PREVIEW_PATH, "mail_preview"),
            (
                "/_autumn/mail/messages/{message_id}",
                "mail_preview_message",
            ),
            (
                "/_autumn/mail/previews/{mailer}/{method}",
                "mail_preview_template",
            ),
        ] {
            infos.push(RouteInfo::framework_get(path.to_owned(), handler));
        }
    }

    // Framework-owned RFC 8058 one-click unsubscribe endpoint. Runtime merges
    // `unsubscribe_router()` — a GET and a POST at `UNSUBSCRIBE_PATH` — under the
    // same `mail`-feature gate and `should_mount_unsubscribe_endpoint()` predicate
    // that folds the path into `csrf.exempt_paths`, so list both methods here to
    // keep the manifest in lockstep. Without the POST listed, `build_csrf_dimension`
    // would silently under-report the exempt (not-enforced) mutating route.
    #[cfg(feature = "mail")]
    if config.mail.should_mount_unsubscribe_endpoint() {
        for http_method in ["GET", "POST"] {
            infos.push(RouteInfo::framework_route(
                http_method,
                crate::mail::UNSUBSCRIBE_PATH.to_owned(),
                "unsubscribe",
            ));
        }
    }

    // Widget story gallery routes (#1526), listed iff the resolved config
    // enables them (same gating condition as the router mount).
    #[cfg(feature = "maud")]
    if config.stories.enabled {
        for (path, handler) in [
            (crate::stories::STORIES_PATH, "story_gallery_index"),
            ("/_stories/{slug}", "story_gallery_story"),
        ] {
            infos.push(RouteInfo::framework_get(path.to_owned(), handler));
        }
    }

    // Dev request inspector routes.
    if matches!(config.profile.as_deref(), Some("dev" | "development")) {
        let inspector_path = &config.dev.inspector_path;
        let inspector_detail_path = format!("{inspector_path}/requests/{{id}}");
        for (path, handler) in [
            (inspector_path.as_str(), "inspector_index"),
            (inspector_detail_path.as_str(), "inspector_detail"),
        ] {
            infos.push(RouteInfo::framework_get(path.to_owned(), handler));
        }
    }

    // Tracked-job status endpoint (#1627): runtime mounts
    // `GET /_autumn/jobs/{token}` when `jobs.tracking.route_enabled` (default
    // true), gated identically to the router mount. List it so the manifest
    // reflects the mounted route.
    if config.jobs.tracking.route_enabled {
        infos.push(RouteInfo::framework_get(
            crate::job_tracking::JOB_STATUS_ROUTE_PATH.to_owned(),
            "job_status",
        ));
    }

    // Static file serving is unconditionally mounted at /static.
    infos.push(RouteInfo::framework_get(
        "/static/{*path}".to_owned(),
        "static_files",
    ));
}

/// Append `OpenAPI` documentation routes (`/openapi.json`, `/swagger-ui`).
///
/// Only compiled when the `openapi` feature is enabled.
#[cfg(feature = "openapi")]
pub(crate) fn append_openapi_routes(
    infos: &mut Vec<RouteInfo>,
    openapi: &crate::openapi::OpenApiConfig,
) {
    infos.push(RouteInfo::framework_get(
        openapi.openapi_json_path.clone(),
        "openapi_json",
    ));
    if let Some(ui_path) = &openapi.swagger_ui_path {
        infos.push(RouteInfo::framework_get(ui_path.clone(), "swagger_ui"));
    }
}

/// Append dev live-reload routes (`/__autumn/live-reload`, `/__autumn/live-reload.js`).
///
/// Routes are only appended when the Autumn dev server is active
/// (`AUTUMN_DEV=1` and `AUTUMN_ENV != production`).
pub(crate) fn append_dev_reload_routes(infos: &mut Vec<RouteInfo>) {
    if crate::middleware::dev::is_enabled_with_env(&crate::config::OsEnv) {
        for (path, handler) in [
            (crate::middleware::dev::LIVE_RELOAD_PATH, "dev_live_reload"),
            (
                crate::middleware::dev::LIVE_RELOAD_SCRIPT_PATH,
                "dev_live_reload_js",
            ),
        ] {
            infos.push(RouteInfo::framework_get(path.to_owned(), handler));
        }
    }
}

/// Stable-sort route infos: primary key is path (lexicographic), secondary
/// key is HTTP method (lexicographic). This makes output diff-friendly.
pub(crate) fn sort_route_infos(infos: &mut [RouteInfo]) {
    infos.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));
}

/// Join a scope prefix with a child route path, mirroring axum's nest
/// normalization: `/api` + `/` → `/api`, `/api` + `/posts` → `/api/posts`.
fn join_scope_path(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    if path == "/" || path.is_empty() {
        if prefix.is_empty() {
            "/".to_owned()
        } else {
            prefix.to_owned()
        }
    } else if path.starts_with('/') {
        format!("{prefix}{path}")
    } else {
        format!("{prefix}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AutumnConfig;
    use axum::routing::get;
    use http::Method;

    fn dummy_api_doc() -> crate::openapi::ApiDoc {
        crate::openapi::ApiDoc {
            method: "GET",
            path: "/dummy",
            operation_id: "dummy",
            success_status: 200,
            ..Default::default()
        }
    }

    fn make_route(method: Method, path: &'static str, name: &'static str) -> Route {
        make_route_with(method, path, name, dummy_api_doc())
    }

    /// Build a [`RepositoryApiMeta`](crate::route::RepositoryApiMeta) with the
    /// given `has_policy`, mirroring what the `#[repository]` macro emits for a
    /// `policy = ...` (or bare) auto-API.
    fn repo_meta(has_policy: bool) -> crate::route::RepositoryApiMeta {
        crate::route::RepositoryApiMeta {
            resource_type_name: "Post",
            api_path: "/api/posts",
            has_policy,
            policy_check: None,
            scope_check: None,
        }
    }

    /// Build a [`RepositoryApiMeta`](crate::route::RepositoryApiMeta) for a
    /// `scope = ...` (but no `policy = ...`) auto-API: `has_policy` stays
    /// `false`, but the list handler carries a `scope_check` probe.
    fn repo_meta_scope_only() -> crate::route::RepositoryApiMeta {
        fn probe(_: &crate::authorization::PolicyRegistry) -> bool {
            true
        }
        crate::route::RepositoryApiMeta {
            resource_type_name: "Post",
            api_path: "/api/posts",
            has_policy: false,
            policy_check: None,
            scope_check: Some(probe),
        }
    }

    fn make_repo_route(
        method: Method,
        path: &'static str,
        name: &'static str,
        repository: Option<crate::route::RepositoryApiMeta>,
    ) -> Route {
        let mut route = make_route_with(method, path, name, dummy_api_doc());
        route.repository = repository;
        route
    }

    fn make_route_with(
        method: Method,
        path: &'static str,
        name: &'static str,
        api_doc: crate::openapi::ApiDoc,
    ) -> Route {
        async fn handler() -> &'static str {
            "ok"
        }
        Route {
            method,
            path,
            handler: get(handler),
            name,
            api_doc,
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        }
    }

    // ── collect_route_infos ────────────────────────────────────────────────

    #[test]
    fn collect_route_infos_empty_produces_empty() {
        let infos = collect_route_infos(&[], &[], &[], &[]).unwrap();
        assert!(infos.is_empty());
    }

    #[test]
    fn collect_route_infos_single_user_route() {
        let routes = vec![make_route(Method::GET, "/posts", "list_posts")];
        let sources = vec![RouteSource::User];
        let infos = collect_route_infos(&routes, &sources, &[], &[]).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].method, "GET");
        assert_eq!(infos[0].path, "/posts");
        assert_eq!(infos[0].handler, "list_posts");
        assert_eq!(infos[0].source, RouteSource::User);
        assert!(infos[0].middleware.is_empty());
    }

    #[test]
    fn collect_route_infos_multiple_methods_same_path() {
        let routes = vec![
            make_route(Method::GET, "/posts", "list_posts"),
            make_route(Method::POST, "/posts", "create_post"),
        ];
        let sources = vec![RouteSource::User, RouteSource::User];
        let infos = collect_route_infos(&routes, &sources, &[], &[]).unwrap();
        assert_eq!(infos.len(), 2);
    }

    /// Acceptance criterion for issue #605: `autumn routes` /
    /// `/actuator/routes` must keep reporting the declared effective
    /// method (`PUT`, `PATCH`, `DELETE`) even though HTML browser
    /// submissions transport those mutations as `POST` with a hidden
    /// `_method` override.
    ///
    /// Route metadata is collected at registration time from
    /// `Route::method`, never from any per-request rewrite, so the
    /// listing stays semantically honest regardless of how clients
    /// reach the route.
    #[test]
    fn collect_route_infos_reports_declared_method_for_overridable_routes() {
        let routes = vec![
            make_route(Method::PUT, "/posts/{id}", "update_post"),
            make_route(Method::PATCH, "/posts/{id}", "patch_post"),
            make_route(Method::DELETE, "/posts/{id}", "delete_post"),
        ];
        let sources = vec![RouteSource::User; 3];
        let infos = collect_route_infos(&routes, &sources, &[], &[]).unwrap();
        let methods: Vec<&str> = infos.iter().map(|i| i.method.as_str()).collect();
        assert_eq!(methods, vec!["PUT", "PATCH", "DELETE"]);
        // The transport method browsers actually use must never appear in
        // the listing for these routes.
        assert!(infos.iter().all(|i| i.method != "POST"), "{infos:?}");
    }

    #[test]
    fn collect_route_infos_scoped_group_prepends_prefix() {
        let group = ScopedGroup {
            prefix: "/api".to_owned(),
            routes: vec![make_route(Method::GET, "/posts", "api_list_posts")],
            source: RouteSource::User,
            apply_layer: Box::new(|r| r),
        };
        let infos = collect_route_infos(&[], &[], &[group], &[]).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].path, "/api/posts");
        assert_eq!(infos[0].handler, "api_list_posts");
    }

    #[test]
    fn collect_route_infos_scoped_root_child() {
        let group = ScopedGroup {
            prefix: "/api".to_owned(),
            routes: vec![make_route(Method::GET, "/", "api_root")],
            source: RouteSource::User,
            apply_layer: Box::new(|r| r),
        };
        let infos = collect_route_infos(&[], &[], &[group], &[]).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].path, "/api");
    }

    #[test]
    fn collect_route_infos_marks_user_source() {
        let routes = vec![make_route(Method::POST, "/items", "create_item")];
        let sources = vec![RouteSource::User];
        let infos = collect_route_infos(&routes, &sources, &[], &[]).unwrap();
        assert_eq!(infos[0].source, RouteSource::User);
    }

    #[test]
    fn collect_route_infos_plugin_source_from_parallel_slice() {
        let routes = vec![make_route(Method::GET, "/admin", "admin_index")];
        let sources = vec![RouteSource::Plugin("admin".to_owned())];
        let infos = collect_route_infos(&routes, &sources, &[], &[]).unwrap();
        assert_eq!(infos[0].source, RouteSource::Plugin("admin".to_owned()));
    }

    #[test]
    fn collect_route_infos_plugin_source_on_scoped_group() {
        let group = ScopedGroup {
            prefix: "/admin".to_owned(),
            routes: vec![make_route(Method::GET, "/users", "admin_users")],
            source: RouteSource::Plugin("admin".to_owned()),
            apply_layer: Box::new(|r| r),
        };
        let infos = collect_route_infos(&[], &[], &[group], &[]).unwrap();
        assert_eq!(infos[0].source, RouteSource::Plugin("admin".to_owned()));
        assert_eq!(infos[0].path, "/admin/users");
    }

    #[test]
    fn collect_route_infos_missing_source_defaults_to_user() {
        let routes = vec![make_route(Method::GET, "/x", "x")];
        // empty sources slice — should fall back to User
        let infos = collect_route_infos(&routes, &[], &[], &[]).unwrap();
        assert_eq!(infos[0].source, RouteSource::User);
    }

    // ── classification (#1604) ──────────────────────────────────────────────

    #[test]
    fn classify_framework_source_is_framework() {
        let (c, roles, scopes, policy) = classify(&RouteSource::Framework, &dummy_api_doc(), None);
        assert_eq!(c, RouteClassification::Framework);
        assert!(roles.is_empty() && scopes.is_empty() && !policy);
    }

    #[test]
    fn classify_secured_is_gated_and_carries_posture() {
        let api_doc = crate::openapi::ApiDoc {
            secured: true,
            required_roles: &["admin"],
            required_scopes: &["posts:write"],
            ..dummy_api_doc()
        };
        let (c, roles, scopes, policy) = classify(&RouteSource::User, &api_doc, None);
        assert_eq!(c, RouteClassification::Gated);
        assert_eq!(roles, vec!["admin"]);
        assert_eq!(scopes, vec!["posts:write"]);
        assert!(!policy);
    }

    #[test]
    fn classify_policy_is_gated_and_carries_policy_flag() {
        let api_doc = crate::openapi::ApiDoc {
            has_policy: true,
            ..dummy_api_doc()
        };
        let (c, _roles, _scopes, policy) = classify(&RouteSource::User, &api_doc, None);
        assert_eq!(c, RouteClassification::Gated);
        assert!(policy);
    }

    #[test]
    fn classify_public_is_public() {
        let api_doc = crate::openapi::ApiDoc {
            public: true,
            ..dummy_api_doc()
        };
        let (c, _, _, _) = classify(&RouteSource::User, &api_doc, None);
        assert_eq!(c, RouteClassification::Public);
    }

    #[test]
    fn classify_unannotated_is_unclassified() {
        let (c, _, _, _) = classify(&RouteSource::User, &dummy_api_doc(), None);
        assert_eq!(c, RouteClassification::Unclassified);
    }

    /// A repository CRUD route generated by `#[repository(api = ..., policy =
    /// ...)]` carries its record-level authorization guard on
    /// [`RepositoryApiMeta::has_policy`](crate::route::RepositoryApiMeta),
    /// while its handler `ApiDoc` is left at defaults (never `secured`/
    /// `has_policy`). Such a route must classify `Gated` — not
    /// `Unclassified` — so the audit gate does not flag a policy-protected
    /// endpoint as unauthenticated. (#1604)
    #[test]
    fn classify_repository_policy_is_gated() {
        let repo = repo_meta(true);
        let (c, roles, scopes, policy) =
            classify(&RouteSource::User, &dummy_api_doc(), Some(&repo));
        assert_eq!(c, RouteClassification::Gated);
        assert!(
            policy,
            "repository has_policy must surface as policy = true"
        );
        assert!(roles.is_empty() && scopes.is_empty());
    }

    /// A repository auto-API declared with `scope = ...` (but no `policy =
    /// ...`) enforces the registered scope on its generated list handler,
    /// recorded as `RepositoryApiMeta::scope_check = Some(..)` while
    /// `has_policy` stays `false`. Such a route must classify `Gated` — not
    /// `Unclassified` — so the audit gate does not flag a scope-protected list
    /// endpoint as unauthenticated. No scope *name* is recorded on the meta, so
    /// `scopes` stays empty and `policy` stays `false` (a scope is not a
    /// policy). (#1604)
    #[test]
    fn classify_repository_scope_only_is_gated() {
        let repo = repo_meta_scope_only();
        let (c, roles, scopes, policy) =
            classify(&RouteSource::User, &dummy_api_doc(), Some(&repo));
        assert_eq!(c, RouteClassification::Gated);
        assert!(
            !policy,
            "a repository scope guard is not a policy: policy must stay false"
        );
        assert!(
            roles.is_empty() && scopes.is_empty(),
            "no scope name is recorded on the meta, so scopes stays empty"
        );
    }

    /// A repository route without a policy (`#[repository(api = ...)]`, no
    /// `policy = ...`) has `has_policy == false` and stays `Unclassified`:
    /// that unauthenticated CRUD form is exactly what the audit gate should
    /// keep flagging. (#1604)
    #[test]
    fn classify_repository_without_policy_is_unclassified() {
        let repo = repo_meta(false);
        let (c, _, _, policy) = classify(&RouteSource::User, &dummy_api_doc(), Some(&repo));
        assert_eq!(c, RouteClassification::Unclassified);
        assert!(!policy);
    }

    /// Keystone falsifiability check (#1604) at the library level: an
    /// unannotated mutating handler classifies as `Unclassified` (the audit
    /// gate's failure state), and adding *either* a `#[secured]` guard or a
    /// `#[public]` marker flips it to a passing classification.
    #[test]
    fn unclassified_route_turns_green_when_guarded_or_public() {
        // Red: no auth posture declared.
        let route = make_route_with(Method::POST, "/widgets", "create_widget", dummy_api_doc());
        let infos = collect_route_infos(&[route], &[RouteSource::User], &[], &[]).unwrap();
        assert_eq!(infos[0].classification, RouteClassification::Unclassified);

        // Green via #[secured]: carries the declared role.
        let secured_doc = crate::openapi::ApiDoc {
            secured: true,
            required_roles: &["admin"],
            ..dummy_api_doc()
        };
        let route = make_route_with(Method::POST, "/widgets", "create_widget", secured_doc);
        let infos = collect_route_infos(&[route], &[RouteSource::User], &[], &[]).unwrap();
        assert_eq!(infos[0].classification, RouteClassification::Gated);
        assert_eq!(infos[0].roles, vec!["admin"]);

        // Green via #[public].
        let public_doc = crate::openapi::ApiDoc {
            public: true,
            ..dummy_api_doc()
        };
        let route = make_route_with(Method::POST, "/widgets", "create_widget", public_doc);
        let infos = collect_route_infos(&[route], &[RouteSource::User], &[], &[]).unwrap();
        assert_eq!(infos[0].classification, RouteClassification::Public);
    }

    /// End-to-end through `collect_route_infos`: a policy-protected repository
    /// CRUD route classifies `Gated` (carrying `policy = true`) rather than
    /// `Unclassified`, so it does not fail the audit gate. A bare repository
    /// route (no policy) stays `Unclassified`. (#1604)
    #[test]
    fn collect_repository_policy_route_is_gated() {
        let route = make_repo_route(
            Method::POST,
            "/api/posts",
            "posts_create",
            Some(repo_meta(true)),
        );
        let infos = collect_route_infos(&[route], &[RouteSource::User], &[], &[]).unwrap();
        assert_eq!(infos[0].classification, RouteClassification::Gated);
        assert!(infos[0].policy);

        let bare = make_repo_route(
            Method::POST,
            "/api/posts",
            "posts_create",
            Some(repo_meta(false)),
        );
        let infos = collect_route_infos(&[bare], &[RouteSource::User], &[], &[]).unwrap();
        assert_eq!(infos[0].classification, RouteClassification::Unclassified);
        assert!(!infos[0].policy);
    }

    #[test]
    fn collect_carries_handler_module_when_present() {
        let api_doc = crate::openapi::ApiDoc {
            public: true,
            module_path: "myapp::widgets",
            ..dummy_api_doc()
        };
        let route = make_route_with(Method::GET, "/widgets", "list_widgets", api_doc);
        let infos = collect_route_infos(&[route], &[RouteSource::User], &[], &[]).unwrap();
        assert_eq!(infos[0].module.as_deref(), Some("myapp::widgets"));
    }

    #[test]
    fn framework_get_helper_is_exempt() {
        let info = RouteInfo::framework_get("/actuator/health".to_owned(), "actuator");
        assert_eq!(info.classification, RouteClassification::Framework);
        assert_eq!(info.source, RouteSource::Framework);
        assert_eq!(info.method, "GET");
    }

    #[test]
    fn route_classification_serializes_to_lowercase_tag() {
        assert_eq!(
            serde_json::to_string(&RouteClassification::Gated).unwrap(),
            "\"gated\""
        );
        assert_eq!(
            serde_json::to_string(&RouteClassification::Unclassified).unwrap(),
            "\"unclassified\""
        );
        let decoded: RouteClassification = serde_json::from_str("\"public\"").unwrap();
        assert_eq!(decoded, RouteClassification::Public);
    }

    // ── sort_route_infos ───────────────────────────────────────────────────

    #[test]
    fn sort_route_infos_by_path_then_method() {
        let mut infos = vec![
            RouteInfo {
                method: "POST".to_owned(),
                path: "/posts".to_owned(),
                handler: "create".to_owned(),
                source: RouteSource::User,
                middleware: vec![],
                api_version: None,
                status: None,
                sunset_opt_out: None,
                ..Default::default()
            },
            RouteInfo {
                method: "GET".to_owned(),
                path: "/posts".to_owned(),
                handler: "list".to_owned(),
                source: RouteSource::User,
                middleware: vec![],
                api_version: None,
                status: None,
                sunset_opt_out: None,
                ..Default::default()
            },
            RouteInfo {
                method: "GET".to_owned(),
                path: "/about".to_owned(),
                handler: "about".to_owned(),
                source: RouteSource::User,
                middleware: vec![],
                api_version: None,
                status: None,
                sunset_opt_out: None,
                ..Default::default()
            },
        ];
        sort_route_infos(&mut infos);
        assert_eq!(infos[0].path, "/about");
        assert_eq!(infos[1].path, "/posts");
        assert_eq!(infos[1].method, "GET");
        assert_eq!(infos[2].path, "/posts");
        assert_eq!(infos[2].method, "POST");
    }

    #[test]
    fn sort_route_infos_stable_on_equal() {
        let mut infos = vec![
            RouteInfo {
                method: "GET".to_owned(),
                path: "/z".to_owned(),
                handler: "z".to_owned(),
                source: RouteSource::User,
                middleware: vec![],
                api_version: None,
                status: None,
                sunset_opt_out: None,
                ..Default::default()
            },
            RouteInfo {
                method: "GET".to_owned(),
                path: "/a".to_owned(),
                handler: "a".to_owned(),
                source: RouteSource::User,
                middleware: vec![],
                api_version: None,
                status: None,
                sunset_opt_out: None,
                ..Default::default()
            },
        ];
        sort_route_infos(&mut infos);
        assert_eq!(infos[0].path, "/a");
        assert_eq!(infos[1].path, "/z");
    }

    // ── RouteSource serialization ──────────────────────────────────────────

    #[test]
    fn route_source_user_serializes_to_string() {
        let s = serde_json::to_string(&RouteSource::User).unwrap();
        assert_eq!(s, "\"user\"");
    }

    #[test]
    fn route_source_framework_serializes_to_string() {
        let s = serde_json::to_string(&RouteSource::Framework).unwrap();
        assert_eq!(s, "\"framework\"");
    }

    #[test]
    fn route_source_plugin_serializes_with_name() {
        let s = serde_json::to_string(&RouteSource::Plugin("admin".to_owned())).unwrap();
        assert_eq!(s, "\"plugin:admin\"");
    }

    #[test]
    fn route_source_roundtrips_user() {
        let original = RouteSource::User;
        let json = serde_json::to_string(&original).unwrap();
        let decoded: RouteSource = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn route_source_roundtrips_plugin() {
        let original = RouteSource::Plugin("harvest".to_owned());
        let json = serde_json::to_string(&original).unwrap();
        let decoded: RouteSource = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn route_source_roundtrips_framework() {
        let original = RouteSource::Framework;
        let json = serde_json::to_string(&original).unwrap();
        let decoded: RouteSource = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, original);
    }

    // ── append_framework_routes ────────────────────────────────────────────

    #[test]
    fn append_framework_routes_includes_probe_paths() {
        let config = AutumnConfig::default();
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &config);
        let paths: Vec<&str> = infos.iter().map(|i| i.path.as_str()).collect();
        assert!(
            paths.contains(&config.health.path.as_str()),
            "health path missing: {paths:?}"
        );
        assert!(
            paths.contains(&config.health.live_path.as_str()),
            "live path missing: {paths:?}"
        );
        assert!(
            paths.contains(&config.health.ready_path.as_str()),
            "ready path missing: {paths:?}"
        );
        assert!(
            paths.contains(&config.health.startup_path.as_str()),
            "startup path missing: {paths:?}"
        );
    }

    #[test]
    fn append_framework_routes_marks_framework_source() {
        let config = AutumnConfig::default();
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &config);
        for info in &infos {
            assert_eq!(
                info.source,
                RouteSource::Framework,
                "expected Framework source for {}: {:?}",
                info.path,
                info.source
            );
        }
    }

    #[test]
    fn append_framework_routes_custom_health_path() {
        let mut config = AutumnConfig::default();
        config.health.path = "/ping".to_owned();
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &config);
        let paths: Vec<&str> = infos.iter().map(|i| i.path.as_str()).collect();
        assert!(
            paths.contains(&"/ping"),
            "custom health path missing: {paths:?}"
        );
    }

    /// Codex P2 (issue #1627): mutating actuator routes must be enumerated with
    /// their real HTTP method so the `csrf` posture dimension can see them.
    /// `PUT {prefix}/loggers/{name}` is mounted at runtime when
    /// `actuator.sensitive = true` but was previously absent from the listing.
    #[test]
    fn append_framework_routes_includes_mutating_actuator_routes() {
        let mut config = AutumnConfig::default();
        config.actuator.sensitive = true;
        let prefix = config.actuator.prefix.clone();
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &config);

        let loggers_path = format!("{prefix}/loggers/{{name}}");
        assert!(
            infos.iter().any(|i| i.method == "PUT"
                && i.path == loggers_path
                && i.classification == RouteClassification::Framework),
            "expected PUT {loggers_path} framework route: {infos:?}"
        );

        // The `/webhooks/replay` route is mounted as POST only; it must never
        // appear as a phantom GET.
        let replay_path = format!("{prefix}/webhooks/replay");
        assert!(
            !infos
                .iter()
                .any(|i| i.method == "GET" && i.path == replay_path),
            "phantom GET {replay_path} must not be listed: {infos:?}"
        );
    }

    /// Companion to the above, guarded on `http-client`: the DLQ replay endpoint
    /// is mounted as `POST` and must be enumerated as such.
    #[cfg(feature = "http-client")]
    #[test]
    fn append_framework_routes_includes_webhook_replay_post() {
        let mut config = AutumnConfig::default();
        config.actuator.sensitive = true;
        let prefix = config.actuator.prefix.clone();
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &config);

        let replay_path = format!("{prefix}/webhooks/replay");
        assert!(
            infos.iter().any(|i| i.method == "POST"
                && i.path == replay_path
                && i.classification == RouteClassification::Framework),
            "expected POST {replay_path} framework route: {infos:?}"
        );
        // `/webhooks/dlq` stays a GET listing.
        let dlq_path = format!("{prefix}/webhooks/dlq");
        assert!(
            infos
                .iter()
                .any(|i| i.method == "GET" && i.path == dlq_path),
            "expected GET {dlq_path} framework route: {infos:?}"
        );
    }

    /// Regression guard (#1627): `/webhooks/replay` must stay in the runtime
    /// path set (`actuator_endpoint_paths`) that seeds the startup-barrier
    /// allow-list, while the GET-only route listing must NOT emit a phantom
    /// GET for it (only the real `POST`). A prior fix dropped the path from
    /// `actuator_endpoint_paths` entirely to kill the phantom GET, which also
    /// silently removed it from the barrier bypass set — this asserts both
    /// halves are decoupled: complete runtime set, clean listing.
    #[cfg(feature = "http-client")]
    #[test]
    fn webhook_replay_in_runtime_set_but_not_a_phantom_get_listing() {
        let mut config = AutumnConfig::default();
        config.actuator.sensitive = true;
        let prefix = config.actuator.prefix.clone();
        let replay_path = format!("{prefix}/webhooks/replay");

        // Runtime set (barrier allow-list source) still contains the path.
        let runtime_paths = crate::actuator::actuator_endpoint_paths(
            &prefix,
            config.actuator.sensitive,
            config.actuator.prometheus,
        );
        assert!(
            runtime_paths.contains(&replay_path),
            "runtime actuator_endpoint_paths must contain {replay_path} so the \
             startup barrier bypasses the POST: {runtime_paths:?}"
        );

        // Listing: no phantom GET, but the real POST is present.
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &config);
        assert!(
            !infos
                .iter()
                .any(|i| i.method == "GET" && i.path == replay_path),
            "listing must not contain a phantom GET {replay_path}: {infos:?}"
        );
        assert!(
            infos
                .iter()
                .any(|i| i.method == "POST" && i.path == replay_path),
            "listing must contain POST {replay_path}: {infos:?}"
        );
    }

    /// #1627: the tracked-job status endpoint (`GET /_autumn/jobs/{token}`) is
    /// mounted by default and must be enumerated; disabling
    /// `jobs.tracking.route_enabled` (the same gate as the router mount) drops
    /// both the mount and the listing.
    #[test]
    fn framework_routes_include_job_status_when_enabled() {
        let mut config = AutumnConfig::default();
        assert!(
            config.jobs.tracking.route_enabled,
            "job status route should default to enabled"
        );
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &config);
        assert!(
            infos.iter().any(|i| i.method == "GET"
                && i.path == crate::job_tracking::JOB_STATUS_ROUTE_PATH
                && i.classification == RouteClassification::Framework),
            "expected GET {} framework route when enabled: {infos:?}",
            crate::job_tracking::JOB_STATUS_ROUTE_PATH
        );

        config.jobs.tracking.route_enabled = false;
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &config);
        assert!(
            !infos
                .iter()
                .any(|i| i.path == crate::job_tracking::JOB_STATUS_ROUTE_PATH),
            "job status route must be absent when disabled: {infos:?}"
        );
    }

    /// T18 (AC5, issue #1526): `autumn routes` introspection lists the story
    /// gallery endpoints exactly when the resolved config enables them.
    #[cfg(feature = "maud")]
    #[test]
    fn framework_routes_include_stories_when_enabled() {
        let mut config = AutumnConfig::default();
        config.stories.enabled = true;
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &config);
        let paths: Vec<&str> = infos.iter().map(|i| i.path.as_str()).collect();
        assert!(
            paths.contains(&crate::stories::STORIES_PATH),
            "enabled stories must list the index route: {paths:?}"
        );
        assert!(
            paths.contains(&"/_stories/{slug}"),
            "enabled stories must list the detail route: {paths:?}"
        );

        let default_config = AutumnConfig::default();
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &default_config);
        let paths: Vec<&str> = infos.iter().map(|i| i.path.as_str()).collect();
        assert!(
            !paths.contains(&"/_stories"),
            "disabled stories must not be listed: {paths:?}"
        );
        assert!(
            !paths.contains(&"/_stories/{slug}"),
            "disabled stories must not list the detail route: {paths:?}"
        );
    }

    /// Codex P2 (issue #1627): when the RFC 8058 one-click unsubscribe endpoint
    /// is mounted, runtime merges `unsubscribe_router()` — a GET and a POST at
    /// `UNSUBSCRIBE_PATH` — and exempts the path from CSRF. `append_framework_routes`
    /// must list both methods (matching the same `should_mount_unsubscribe_endpoint()`
    /// predicate the exempt fold uses) so the csrf posture dimension can see the
    /// mutating POST. When the endpoint is not mounted, neither route is listed.
    #[cfg(feature = "mail")]
    #[test]
    fn append_framework_routes_includes_mounted_unsubscribe_routes() {
        let mut config = AutumnConfig::default();
        config.mail.mount_unsubscribe_endpoint = true;
        config.mail.unsubscribe_base_url = Some("https://example.com".to_owned());
        assert!(
            config.mail.should_mount_unsubscribe_endpoint(),
            "test precondition: unsubscribe endpoint must be mounted"
        );
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &config);

        assert!(
            infos.iter().any(|i| i.method == "POST"
                && i.path == crate::mail::UNSUBSCRIBE_PATH
                && i.classification == RouteClassification::Framework),
            "expected POST {} framework route: {infos:?}",
            crate::mail::UNSUBSCRIBE_PATH
        );
        assert!(
            infos.iter().any(|i| i.method == "GET"
                && i.path == crate::mail::UNSUBSCRIBE_PATH
                && i.classification == RouteClassification::Framework),
            "expected GET {} framework route: {infos:?}",
            crate::mail::UNSUBSCRIBE_PATH
        );

        // When the endpoint is NOT mounted, neither route is listed.
        let plain = AutumnConfig::default();
        assert!(!plain.mail.should_mount_unsubscribe_endpoint());
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &plain);
        assert!(
            !infos
                .iter()
                .any(|i| i.path == crate::mail::UNSUBSCRIBE_PATH),
            "unmounted unsubscribe path must not be listed: {infos:?}"
        );
    }

    // ── join_scope_path ────────────────────────────────────────────────────

    #[test]
    fn join_scope_path_normal() {
        assert_eq!(join_scope_path("/api", "/posts"), "/api/posts");
    }

    #[test]
    fn join_scope_path_root_child() {
        assert_eq!(join_scope_path("/api", "/"), "/api");
    }

    #[test]
    fn join_scope_path_empty_child() {
        assert_eq!(join_scope_path("/api", ""), "/api");
    }

    #[test]
    fn join_scope_path_trailing_slash_on_prefix() {
        assert_eq!(join_scope_path("/api/", "/posts"), "/api/posts");
    }

    #[test]
    fn join_scope_path_empty_prefix() {
        assert_eq!(join_scope_path("", "/posts"), "/posts");
    }

    #[test]
    fn join_scope_path_root_prefix_root_child() {
        assert_eq!(join_scope_path("", "/"), "/");
    }

    // ── RouteInfo JSON roundtrip ───────────────────────────────────────────

    #[test]
    fn route_info_roundtrips_json() {
        let info = RouteInfo {
            method: "GET".to_owned(),
            path: "/posts/{id}".to_owned(),
            handler: "posts::show".to_owned(),
            source: RouteSource::User,
            middleware: vec!["secured".to_owned()],
            api_version: None,
            status: None,
            sunset_opt_out: None,
            ..Default::default()
        };
        let json = serde_json::to_string(&info).unwrap();
        let decoded: RouteInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.method, "GET");
        assert_eq!(decoded.path, "/posts/{id}");
        assert_eq!(decoded.handler, "posts::show");
        assert_eq!(decoded.source, RouteSource::User);
        assert_eq!(decoded.middleware, vec!["secured"]);
    }

    // ── append_openapi_routes ──────────────────────────────────────────────

    #[cfg(feature = "openapi")]
    #[test]
    fn append_openapi_routes_adds_json_and_ui_paths() {
        let config = crate::openapi::OpenApiConfig::new("Test", "1.0.0");
        let mut infos = Vec::new();
        append_openapi_routes(&mut infos, &config);
        let paths: Vec<&str> = infos.iter().map(|i| i.path.as_str()).collect();
        assert!(
            paths.contains(&"/openapi.json"),
            "openapi json path missing: {paths:?}"
        );
        assert!(
            paths.contains(&"/swagger-ui"),
            "swagger ui path missing: {paths:?}"
        );
        for info in &infos {
            assert_eq!(info.source, RouteSource::Framework);
            assert_eq!(info.method, "GET");
        }
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn append_openapi_routes_custom_paths() {
        let config = crate::openapi::OpenApiConfig::new("Test", "1.0.0")
            .openapi_json_path("/docs/openapi.json")
            .swagger_ui_path(Some("/docs/ui".to_owned()));
        let mut infos = Vec::new();
        append_openapi_routes(&mut infos, &config);
        let paths: Vec<&str> = infos.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&"/docs/openapi.json"));
        assert!(paths.contains(&"/docs/ui"));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn append_openapi_routes_no_swagger_ui_when_none() {
        let config = crate::openapi::OpenApiConfig::new("Test", "1.0.0").swagger_ui_path(None);
        let mut infos = Vec::new();
        append_openapi_routes(&mut infos, &config);
        // Only the JSON endpoint; no swagger-ui entry.
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].path, "/openapi.json");
    }

    // ── append_framework_routes static ────────────────────────────────────

    #[test]
    fn append_framework_routes_includes_static_catch_all() {
        let config = AutumnConfig::default();
        let mut infos = Vec::new();
        append_framework_routes(&mut infos, &config);
        let static_route = infos.iter().find(|r| r.path == "/static/{*path}");
        assert!(
            static_route.is_some(),
            "framework routes should include /static/{{*path}}"
        );
        let r = static_route.unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.handler, "static_files");
        assert_eq!(r.source, RouteSource::Framework);
    }

    // ── append_dev_reload_routes ───────────────────────────────────────────

    #[test]
    fn append_dev_reload_routes_empty_when_dev_disabled() {
        // In the test environment AUTUMN_DEV is not set, so this should be a no-op.
        let guard = std::env::var("AUTUMN_DEV");
        if guard.is_ok() {
            // Skip: dev mode is active in this test process.
            return;
        }
        let mut infos = Vec::new();
        append_dev_reload_routes(&mut infos);
        assert!(
            infos.is_empty(),
            "expected no dev routes when AUTUMN_DEV unset"
        );
    }

    // ── SecurityDump::from_config (declared-vs-runtime honesty) ─────────────

    /// Finding A: when CSP nonce injection is enabled and the CSP is the
    /// framework default, runtime emits the nonce-aware template (not the raw
    /// default), so the dumped CSP must be that template with the stable
    /// `AUTUMN_CSP_NONCE` placeholder — never the raw default.
    #[test]
    fn security_dump_reports_nonce_aware_csp_when_nonce_enabled() {
        let mut config = AutumnConfig::default();
        // Default CSP is left in place; only the nonce flag flips.
        config.security.headers.csp_nonce.enabled = true;

        let dump = SecurityDump::from_config(&config);
        let csp = &dump.headers.content_security_policy;
        assert!(
            csp.contains("'nonce-AUTUMN_CSP_NONCE'"),
            "nonce-enabled default CSP must report the nonce-aware template: {csp}"
        );
        assert_eq!(
            *csp,
            crate::security::headers::resolved_content_security_policy(&config.security.headers),
            "dump must mirror the runtime CSP resolution verbatim"
        );

        // With the nonce disabled the raw default is reported (no placeholder).
        let mut plain = AutumnConfig::default();
        plain.security.headers.csp_nonce.enabled = false;
        let plain_dump = SecurityDump::from_config(&plain);
        assert!(
            !plain_dump
                .headers
                .content_security_policy
                .contains("AUTUMN_CSP_NONCE"),
            "nonce-disabled CSP must not carry the placeholder"
        );
    }

    /// Finding B: runtime's `apply_csrf_middleware` exempts every configured
    /// webhook endpoint path, so the dumped `csrf.exempt_paths` must include
    /// them even when they are not duplicated in `security.csrf.exempt_paths`.
    #[test]
    fn security_dump_exempts_configured_webhook_paths() {
        let mut config = AutumnConfig::default();
        config.security.webhooks.endpoints = vec![crate::webhook::WebhookEndpointConfig {
            path: "/webhooks/stripe".to_owned(),
            ..Default::default()
        }];

        let dump = SecurityDump::from_config(&config);
        assert!(
            dump.csrf
                .exempt_paths
                .iter()
                .any(|p| p == "/webhooks/stripe"),
            "configured webhook path must be a CSRF exempt path: {:?}",
            dump.csrf.exempt_paths
        );
        // Determinism: sorted and deduped.
        let mut sorted = dump.csrf.exempt_paths.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            dump.csrf.exempt_paths, sorted,
            "exempt_paths must stay sorted and deduped"
        );
    }

    /// Runtime's `apply_csrf_middleware` exempts the framework-owned RFC 8058
    /// one-click unsubscribe endpoint when the app opts in with a base URL, so
    /// the dumped `csrf.exempt_paths` must include `UNSUBSCRIBE_PATH` for that
    /// configuration and a mutating route there must read as CSRF-unenforced.
    #[cfg(feature = "mail")]
    #[test]
    fn security_dump_exempts_mounted_unsubscribe_path() {
        let mut config = AutumnConfig::default();
        config.mail.mount_unsubscribe_endpoint = true;
        config.mail.unsubscribe_base_url = Some("https://example.com".to_owned());
        assert!(
            config.mail.should_mount_unsubscribe_endpoint(),
            "test precondition: unsubscribe endpoint must be mounted"
        );

        let dump = SecurityDump::from_config(&config);
        assert!(
            dump.csrf
                .exempt_paths
                .iter()
                .any(|p| p == crate::mail::UNSUBSCRIBE_PATH),
            "mounted unsubscribe path must be a CSRF exempt path: {:?}",
            dump.csrf.exempt_paths
        );

        // When the endpoint is NOT mounted, the path must not be folded in.
        let mut plain = AutumnConfig::default();
        plain.mail.mount_unsubscribe_endpoint = false;
        let plain_dump = SecurityDump::from_config(&plain);
        assert!(
            !plain_dump
                .csrf
                .exempt_paths
                .iter()
                .any(|p| p == crate::mail::UNSUBSCRIBE_PATH),
            "unmounted unsubscribe path must not be exempt: {:?}",
            plain_dump.csrf.exempt_paths
        );
    }
}
