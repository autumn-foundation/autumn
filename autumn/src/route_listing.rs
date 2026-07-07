//! Route listing types and collection logic for `autumn routes`.
//!
//! Collects route metadata from an [`AppBuilder`](crate::app::AppBuilder) into
//! serializable [`RouteInfo`] values that the CLI can consume without booting
//! the full HTTP server.

use serde::{Deserialize, Serialize};

use crate::app::ScopedGroup;
use crate::route::Route;

/// Where a route was registered: by the user application, by a named plugin,
/// or by the framework itself (probes, actuator, htmx assets, dev reload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteSource {
    /// Registered directly by the user application.
    User,
    /// Registered by a named autumn plugin (e.g. `"admin"` for autumn-admin-plugin).
    Plugin(String),
    /// Registered by the framework (probes, actuator, htmx assets, dev reload).
    Framework,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        infos.push(RouteInfo {
            method: route.method.to_string(),
            path: route.path.to_owned(),
            handler: route.name.to_owned(),
            source,
            middleware: Vec::new(),
            api_version,
            status,
            sunset_opt_out,
        });
    }

    for group in scoped_groups {
        for route in &group.routes {
            let full_path = join_scope_path(&group.prefix, route.path);
            let (api_version, status, sunset_opt_out) =
                resolve_status(route.name, route.api_version, route.sunset_opt_out)?;
            infos.push(RouteInfo {
                method: route.method.to_string(),
                path: full_path,
                handler: route.name.to_owned(),
                source: group.source.clone(),
                middleware: Vec::new(),
                api_version,
                status,
                sunset_opt_out,
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
            infos.push(RouteInfo {
                method: "GET".to_owned(),
                path: path.to_owned(),
                handler: name.to_owned(),
                source: RouteSource::Framework,
                middleware: Vec::new(),
                api_version: None,
                status: None,
                sunset_opt_out: None,
            });
        }
    }

    for path in crate::actuator::actuator_endpoint_paths(
        &config.actuator.prefix,
        config.actuator.sensitive,
        config.actuator.prometheus,
    ) {
        infos.push(RouteInfo {
            method: "GET".to_owned(),
            path,
            handler: "actuator".to_owned(),
            source: RouteSource::Framework,
            middleware: Vec::new(),
            api_version: None,
            status: None,
            sunset_opt_out: None,
        });
    }

    #[cfg(feature = "htmx")]
    {
        infos.push(RouteInfo {
            method: "GET".to_owned(),
            path: crate::htmx::HTMX_JS_PATH.to_owned(),
            handler: "htmx".to_owned(),
            source: RouteSource::Framework,
            middleware: Vec::new(),
            api_version: None,
            status: None,
            sunset_opt_out: None,
        });
        infos.push(RouteInfo {
            method: "GET".to_owned(),
            path: crate::htmx::HTMX_CSRF_JS_PATH.to_owned(),
            handler: "htmx_csrf".to_owned(),
            source: RouteSource::Framework,
            middleware: Vec::new(),
            api_version: None,
            status: None,
            sunset_opt_out: None,
        });
        infos.push(RouteInfo {
            method: "GET".to_owned(),
            path: crate::htmx::IDIOMORPH_JS_PATH.to_owned(),
            handler: "idiomorph".to_owned(),
            source: RouteSource::Framework,
            middleware: Vec::new(),
            api_version: None,
            status: None,
            sunset_opt_out: None,
        });
        infos.push(RouteInfo {
            method: "GET".to_owned(),
            path: crate::htmx::HTMX_SSE_JS_PATH.to_owned(),
            handler: "htmx_sse".to_owned(),
            source: RouteSource::Framework,
            middleware: Vec::new(),
            api_version: None,
            status: None,
            sunset_opt_out: None,
        });
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
            infos.push(RouteInfo {
                method: "GET".to_owned(),
                path: path.to_owned(),
                handler: handler.to_owned(),
                source: RouteSource::Framework,
                middleware: Vec::new(),
                api_version: None,
                status: None,
                sunset_opt_out: None,
            });
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
            infos.push(RouteInfo {
                method: "GET".to_owned(),
                path: path.to_owned(),
                handler: handler.to_owned(),
                source: RouteSource::Framework,
                middleware: Vec::new(),
                api_version: None,
                status: None,
                sunset_opt_out: None,
            });
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
            infos.push(RouteInfo {
                method: "GET".to_owned(),
                path: path.to_owned(),
                handler: handler.to_owned(),
                source: RouteSource::Framework,
                middleware: Vec::new(),
                api_version: None,
                status: None,
                sunset_opt_out: None,
            });
        }
    }

    // Static file serving is unconditionally mounted at /static.
    infos.push(RouteInfo {
        method: "GET".to_owned(),
        path: "/static/{*path}".to_owned(),
        handler: "static_files".to_owned(),
        source: RouteSource::Framework,
        middleware: Vec::new(),
        api_version: None,
        status: None,
        sunset_opt_out: None,
    });
}

/// Append `OpenAPI` documentation routes (`/v3/api-docs`, `/swagger-ui`).
///
/// Only compiled when the `openapi` feature is enabled.
#[cfg(feature = "openapi")]
pub(crate) fn append_openapi_routes(
    infos: &mut Vec<RouteInfo>,
    openapi: &crate::openapi::OpenApiConfig,
) {
    infos.push(RouteInfo {
        method: "GET".to_owned(),
        path: openapi.openapi_json_path.clone(),
        handler: "openapi_json".to_owned(),
        source: RouteSource::Framework,
        middleware: Vec::new(),
        api_version: None,
        status: None,
        sunset_opt_out: None,
    });
    if let Some(ui_path) = &openapi.swagger_ui_path {
        infos.push(RouteInfo {
            method: "GET".to_owned(),
            path: ui_path.clone(),
            handler: "swagger_ui".to_owned(),
            source: RouteSource::Framework,
            middleware: Vec::new(),
            api_version: None,
            status: None,
            sunset_opt_out: None,
        });
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
            infos.push(RouteInfo {
                method: "GET".to_owned(),
                path: path.to_owned(),
                handler: handler.to_owned(),
                source: RouteSource::Framework,
                middleware: Vec::new(),
                api_version: None,
                status: None,
                sunset_opt_out: None,
            });
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
        async fn handler() -> &'static str {
            "ok"
        }
        Route {
            method,
            path,
            handler: get(handler),
            name,
            api_doc: dummy_api_doc(),
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
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
}
