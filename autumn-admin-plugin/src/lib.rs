//! # autumn-admin-plugin
//!
//! Out-of-the-box admin panel plugin for autumn-web applications.
//!
//! Provides auto-generated CRUD views, search, filtering, and audit trails
//! for any model registered via the [`AdminPlugin`] builder. The UI is
//! server-rendered with Maud + HTMX — no JS build step required.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use autumn_admin_plugin::{prelude::*, AdminPlugin};
//!
//! autumn_web::app()
//!     .plugin(
//!         AdminPlugin::new()
//!             .register(ProjectAdmin::default())
//!             .register(TicketAdmin::default()),
//!     )
//!     .routes(routes![...])
//!     .run()
//!     .await;
//! ```
//!
//! # Security
//!
//! The plugin requires the `"admin"` role in the session by default. Override
//! with [`AdminPlugin::require_role`] (pass `None` to disable; not recommended
//! for production).
//!
//! # Naming convention
//!
//! First-party plugin: `autumn-<name>-plugin`.

mod auth;
pub mod experiments;
pub mod feature_flags;
mod registry;
mod routes;
mod templates;
pub mod tokens;
mod traits;

pub use registry::AdminRegistry;
pub use traits::{
    AdminAction, AdminError, AdminField, AdminFieldKind, AdminFuture, AdminHistoryEntry,
    AdminHistoryPage, AdminImportError, AdminImportReport, AdminImportRowResult, AdminModel,
    CsvImportMode, ListParams, ListResult, SelectOption, SortDirection,
};

/// Common downstream imports for implementing admin models.
pub mod prelude {
    pub use crate::{
        AdminError, AdminField, AdminFieldKind, AdminFuture, AdminHistoryEntry, AdminHistoryPage,
        AdminImportRowResult, AdminModel, CsvImportMode, ListParams, ListResult, SelectOption,
        SortDirection,
    };
}

use std::borrow::Cow;
use std::sync::Arc;

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::route_listing::RouteInfo;
use autumn_web::runtime_config::RuntimeConfigService;

/// The admin panel plugin.
///
/// Register models via `.register()` and the plugin will mount a full admin
/// UI under the configured prefix (default: `/admin`).
pub struct AdminPlugin {
    registry: AdminRegistry,
    prefix: String,
    actuator_prefix: String,
    auth_session_key: String,
    require_role: Option<String>,
    runtime_config: Option<Arc<RuntimeConfigService>>,
    /// When `true`, every mutating action (create, update, destroy) on any
    /// registered model requires step-up authentication before proceeding.
    ///
    /// Enables this with [`AdminPlugin::with_step_up_mutations`].
    step_up_mutations: bool,
    /// Freshness window for step-up checks on admin mutations (seconds).
    /// Defaults to [`autumn_web::step_up::DEFAULT_MAX_AGE_SECS`].
    /// Override with [`AdminPlugin::with_step_up_max_age`].
    step_up_max_age_secs: u64,
}

impl AdminPlugin {
    /// Create a new admin plugin with default settings.
    ///
    /// Mounts at `/admin` and requires the `"admin"` role in the session.
    /// Links to the actuator UI under `/actuator`. Reads the user
    /// identifier from session key `"user_id"` (Autumn's default
    /// `auth.session_key`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: AdminRegistry::new(),
            prefix: "/admin".to_owned(),
            actuator_prefix: "/actuator".to_owned(),
            auth_session_key: "user_id".to_owned(),
            require_role: Some("admin".to_owned()),
            runtime_config: None,
            step_up_mutations: false,
            step_up_max_age_secs: autumn_web::step_up::DEFAULT_MAX_AGE_SECS,
        }
    }

    /// Override the URL prefix (default: `/admin`).
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Override the actuator mount prefix that dashboard links/polling target
    /// (default: `/actuator`). Must match `config.actuator.prefix` from your
    /// autumn config — the plugin cannot read it automatically because config
    /// is loaded after `Plugin::build` runs.
    #[must_use]
    pub fn actuator_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.actuator_prefix = prefix.into();
        self
    }

    /// Override the session key the role middleware reads to detect an
    /// authenticated user. Default: `"user_id"`, matching Autumn's default
    /// `auth.session_key`. Must match whatever your application populates
    /// after login — e.g. set this to `"uid"` if you configured
    /// `auth.session_key = "uid"`.
    ///
    /// The plugin can't read `config.auth.session_key` automatically
    /// because config is loaded after `Plugin::build` runs.
    #[must_use]
    pub fn auth_session_key(mut self, key: impl Into<String>) -> Self {
        self.auth_session_key = key.into();
        self
    }

    /// Set the required session role for accessing the admin panel.
    ///
    /// Pass `None` to disable role checks entirely. Authentication
    /// (a populated `user_id` session key) is always required when a role
    /// is set.
    #[must_use]
    pub fn require_role(mut self, role: impl Into<Option<String>>) -> Self {
        self.require_role = role.into();
        self
    }

    /// Register a model for admin management.
    ///
    /// The model must implement [`AdminModel`], which provides field metadata,
    /// CRUD operations, and display configuration.
    #[must_use]
    pub fn register<M: AdminModel>(mut self, model: M) -> Self {
        self.registry.register(model);
        self
    }

    /// Enable the runtime config management page.
    ///
    /// Mounts `GET /config`, `POST /config/{key}/set`, `POST /config/{key}/unset`,
    /// and `GET /config/{key}/history` under the admin prefix, and adds a
    /// "Runtime Config" item to the sidebar navigation.
    #[must_use]
    pub fn with_runtime_config(mut self, svc: Arc<RuntimeConfigService>) -> Self {
        self.runtime_config = Some(svc);
        self
    }

    /// Require step-up (fresh) authentication before any mutating admin action.
    ///
    /// When enabled, every `POST` (create/update) and `DELETE` (destroy) request
    /// to the admin panel is checked against the session's `last_strong_auth_at`
    /// claim using the global step-up max-age configured in `[auth.step_up]`
    /// (default: 5 minutes). Requests without a valid fresh-auth claim are
    /// redirected to `/reauth?return_to=…` (HTML clients) or receive a
    /// `401 step_up_required` problem-details response (JSON clients).
    ///
    /// Highly recommended for production admin panels to reduce the blast radius
    /// of a hijacked admin session.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// AdminPlugin::new()
    ///     .register(UserAdmin::default())
    ///     .with_step_up_mutations()
    /// ```
    #[must_use]
    pub const fn with_step_up_mutations(mut self) -> Self {
        self.step_up_mutations = true;
        self
    }

    /// Override the step-up freshness window for admin mutations.
    ///
    /// Only meaningful when [`with_step_up_mutations`](Self::with_step_up_mutations)
    /// is also called. Calls `with_step_up_mutations` implicitly.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// AdminPlugin::new()
    ///     .register(UserAdmin::default())
    ///     .with_step_up_max_age(600) // 10-minute window
    /// ```
    #[must_use]
    pub const fn with_step_up_max_age(mut self, secs: u64) -> Self {
        self.step_up_mutations = true;
        self.step_up_max_age_secs = secs;
        self
    }
}

impl Default for AdminPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for AdminPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("autumn-admin-plugin")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let Self {
            registry,
            prefix,
            actuator_prefix,
            auth_session_key,
            require_role,
            runtime_config,
            step_up_mutations,
            step_up_max_age_secs,
        } = self;
        let has_config = runtime_config.is_some();
        // "config" slug only conflicts when the runtime-config routes are mounted.
        assert!(
            !(has_config && registry.get("config").is_some()),
            "autumn-admin: model slug 'config' conflicts with the mounted runtime-config \
             routes; rename the model or don't call with_runtime_config",
        );
        let registry = Arc::new(registry);
        let router = routes::admin_router(
            Arc::clone(&registry),
            &prefix,
            actuator_prefix.clone(),
            auth_session_key.clone(),
            require_role.clone(),
            runtime_config,
            step_up_mutations,
            step_up_max_age_secs,
        );

        tracing::info!(
            prefix = %prefix,
            actuator_prefix = %actuator_prefix,
            auth_session_key = %auth_session_key,
            models = registry.model_count(),
            role = require_role.as_deref().unwrap_or("<none>"),
            step_up_mutations = %step_up_mutations,
            "🍂 Autumn Admin mounted"
        );

        // Declare routes for `autumn routes` listing. The underlying Axum router
        // is added via nest() which is opaque to route enumeration, so we
        // explicitly register route metadata here.
        let declared = admin_route_infos(&prefix, has_config);

        app.nest(&prefix, router).declare_plugin_routes(declared)
    }
}

/// Generate the route metadata list for this plugin's mounted routes.
///
/// `has_config` must match whether `with_runtime_config` was called; config
/// routes are only mounted when a `RuntimeConfigService` is provided, so
/// including them unconditionally would produce false route-collision signals.
///
/// Kept in sync with `routes::admin_router` — update here when routes are
/// added or removed from the admin router.
pub(crate) fn admin_route_infos(prefix: &str, has_config: bool) -> Vec<RouteInfo> {
    let mut entries: Vec<(&str, String)> = vec![
        ("GET", prefix.to_string()),
        ("GET", format!("{prefix}/jobs")),
        ("GET", format!("{prefix}/jobs/counters")),
        ("POST", format!("{prefix}/jobs/{{id}}/retry")),
        ("POST", format!("{prefix}/jobs/{{id}}/discard")),
        ("POST", format!("{prefix}/jobs/{{id}}/cancel")),
    ];
    if has_config {
        entries.extend([
            ("GET", format!("{prefix}/config")),
            ("POST", format!("{prefix}/config/{{key}}/set")),
            ("POST", format!("{prefix}/config/{{key}}/unset")),
            ("GET", format!("{prefix}/config/{{key}}/history")),
        ]);
    }
    entries.extend([
        ("GET", format!("{prefix}/{{slug}}")),
        ("POST", format!("{prefix}/{{slug}}")),
        ("GET", format!("{prefix}/{{slug}}/new")),
        ("GET", format!("{prefix}/{{slug}}/export.csv")),
        ("GET", format!("{prefix}/{{slug}}/import")),
        ("POST", format!("{prefix}/{{slug}}/import")),
        ("GET", format!("{prefix}/{{slug}}/{{id}}")),
        ("POST", format!("{prefix}/{{slug}}/{{id}}")),
        ("DELETE", format!("{prefix}/{{slug}}/{{id}}")),
        ("GET", format!("{prefix}/{{slug}}/{{id}}/edit")),
        ("GET", format!("{prefix}/{{slug}}/{{id}}/history")),
        ("POST", format!("{prefix}/{{slug}}/actions")),
        ("GET", format!("{prefix}{}", *routes::ADMIN_JS_PATH)),
    ]);
    entries
        .into_iter()
        .map(|(method, path)| RouteInfo {
            method: method.to_owned(),
            path,
            handler: format!("admin::{}", method.to_lowercase()),
            source: autumn_web::route_listing::RouteSource::User, // overwritten by declare_plugin_routes
            middleware: vec![],
            api_version: None,
            status: None,
            sunset_opt_out: None,
        })
        .collect()
}

// ── Conformance reference tests ────────────────────────────────────────────
//
// These tests are the reference example for the Autumn plugin conformance
// workflow documented in docs/plugins.md.  They use
// `autumn_web::plugin_conformance` to verify that the admin plugin's declared
// routes satisfy all conformance checks before publication.
//
// See docs/plugins.md § "Plugin conformance and publishing checklist" for the
// equivalent `autumn plugin-check` CLI invocation.

#[cfg(test)]
mod conformance_tests {
    use autumn_web::plugin_conformance::{ConformanceConfig, run_conformance};
    use autumn_web::route_listing::{RouteInfo, RouteSource};

    const PLUGIN_NAME: &str = "autumn-admin-plugin";

    /// Build the routes that `AdminPlugin` contributes under `prefix`,
    /// attributed to the plugin. Reuses `admin_route_infos` from the outer
    /// module and overrides the source to `Plugin(PLUGIN_NAME)`.
    fn admin_routes(prefix: &str) -> Vec<RouteInfo> {
        admin_routes_with_config(prefix, true)
    }

    fn admin_routes_with_config(prefix: &str, has_config: bool) -> Vec<RouteInfo> {
        super::admin_route_infos(prefix, has_config)
            .into_iter()
            .map(|mut r| {
                r.source = RouteSource::Plugin(PLUGIN_NAME.to_owned());
                r
            })
            .collect()
    }

    #[test]
    fn admin_plugin_routes_are_attributed_to_plugin_name() {
        let routes = admin_routes("/admin");
        let result = autumn_web::plugin_conformance::check_route_attribution(PLUGIN_NAME, &routes);
        assert_eq!(
            result.status,
            autumn_web::plugin_conformance::CheckStatus::Pass,
            "route attribution failed: {}",
            result.message
        );
    }

    #[test]
    fn admin_plugin_routes_live_under_admin_prefix() {
        let routes = admin_routes("/admin");
        let result =
            autumn_web::plugin_conformance::check_route_prefix(PLUGIN_NAME, "/admin", &[], &routes);
        assert_eq!(
            result.status,
            autumn_web::plugin_conformance::CheckStatus::Pass,
            "prefix check failed: {}\n{:?}",
            result.message,
            result.diagnostics
        );
    }

    #[test]
    fn admin_plugin_declares_builtin_job_routes() {
        let routes = admin_routes("/admin");
        let declared: std::collections::HashSet<(&str, &str)> = routes
            .iter()
            .map(|route| (route.method.as_str(), route.path.as_str()))
            .collect();

        for (method, path) in [
            ("GET", "/admin/jobs"),
            ("GET", "/admin/jobs/counters"),
            ("POST", "/admin/jobs/{id}/retry"),
            ("POST", "/admin/jobs/{id}/discard"),
            ("POST", "/admin/jobs/{id}/cancel"),
        ] {
            assert!(
                declared.contains(&(method, path)),
                "missing declared admin job route {method} {path}"
            );
        }
    }

    #[test]
    fn admin_plugin_declares_builtin_config_routes_when_enabled() {
        let routes = admin_routes_with_config("/admin", true);
        let declared: std::collections::HashSet<(&str, &str)> = routes
            .iter()
            .map(|route| (route.method.as_str(), route.path.as_str()))
            .collect();

        for (method, path) in [
            ("GET", "/admin/config"),
            ("POST", "/admin/config/{key}/set"),
            ("POST", "/admin/config/{key}/unset"),
            ("GET", "/admin/config/{key}/history"),
        ] {
            assert!(
                declared.contains(&(method, path)),
                "missing declared admin config route {method} {path}"
            );
        }
    }

    #[test]
    fn admin_plugin_omits_config_routes_when_disabled() {
        let routes = admin_routes_with_config("/admin", false);
        let declared: std::collections::HashSet<(&str, &str)> = routes
            .iter()
            .map(|route| (route.method.as_str(), route.path.as_str()))
            .collect();

        for (method, path) in [
            ("GET", "/admin/config"),
            ("POST", "/admin/config/{key}/set"),
            ("POST", "/admin/config/{key}/unset"),
            ("GET", "/admin/config/{key}/history"),
        ] {
            assert!(
                !declared.contains(&(method, path)),
                "config route {method} {path} should not be declared when has_config=false"
            );
        }
    }

    #[test]
    fn admin_plugin_has_no_route_collisions_in_isolation() {
        let routes = admin_routes("/admin");
        let (result, _) = autumn_web::plugin_conformance::check_collisions(&routes);
        assert_eq!(
            result.status,
            autumn_web::plugin_conformance::CheckStatus::Pass,
            "unexpected collision: {}\n{:?}",
            result.message,
            result.diagnostics
        );
    }

    #[test]
    fn admin_plugin_sensitive_surfaces_declared_with_role_requirement() {
        let routes = admin_routes("/admin");
        let declared = vec![autumn_web::plugin_conformance::SensitiveRoute {
            path_pattern: "/admin".to_owned(),
            auth_mechanism: "Role: admin required via AdminPlugin::require_role \
                             (default) or AdminPlugin::require_role(None) to disable"
                .to_owned(),
        }];
        let result = autumn_web::plugin_conformance::check_sensitive_surfaces(
            PLUGIN_NAME,
            &routes,
            &declared,
        );
        assert_eq!(
            result.status,
            autumn_web::plugin_conformance::CheckStatus::Pass,
            "sensitive-surfaces check failed: {}",
            result.message
        );
    }

    #[test]
    fn admin_plugin_sensitive_surfaces_fail_without_declaration() {
        let routes = admin_routes("/admin");
        let result =
            autumn_web::plugin_conformance::check_sensitive_surfaces(PLUGIN_NAME, &routes, &[]);
        assert_eq!(
            result.status,
            autumn_web::plugin_conformance::CheckStatus::Fail,
            "expected FAIL when sensitive routes are undeclared"
        );
    }

    #[test]
    fn admin_plugin_passes_full_conformance_with_config() {
        let routes = admin_routes("/admin");
        let config = ConformanceConfig::new(PLUGIN_NAME)
            .prefix("/admin")
            .sensitive_route(
                "/admin",
                "Role: admin required via AdminPlugin::require_role",
            );
        let report = run_conformance(&config, &routes);
        assert!(
            report.passed(),
            "AdminPlugin conformance failed:\n{}",
            report.to_text_report()
        );
    }

    #[test]
    fn admin_plugin_collision_with_host_route_detected() {
        let mut routes = admin_routes("/admin");
        // Simulate a host app that accidentally defines GET /admin
        routes.push(RouteInfo {
            method: "GET".to_owned(),
            path: "/admin".to_owned(),
            handler: "host::admin_redirect".to_owned(),
            source: RouteSource::User,
            middleware: vec![],
            api_version: None,
            status: None,
            sunset_opt_out: None,
        });
        let (result, diagnostics) = autumn_web::plugin_conformance::check_collisions(&routes);
        assert_eq!(
            result.status,
            autumn_web::plugin_conformance::CheckStatus::Fail,
            "expected collision to be detected"
        );
        let diag = &diagnostics[0];
        assert_eq!(diag.method, "GET");
        assert_eq!(diag.path, "/admin");
        let sources: Vec<&str> = diag
            .contributors
            .iter()
            .map(|c| c.source.as_str())
            .collect();
        assert!(
            sources.contains(&"user"),
            "missing user contributor: {sources:?}"
        );
        assert!(
            sources.contains(&"plugin:autumn-admin-plugin"),
            "missing plugin contributor: {sources:?}"
        );
    }

    #[test]
    fn admin_plugin_custom_prefix_passes_conformance() {
        let routes = admin_routes("/backend");
        let config = ConformanceConfig::new(PLUGIN_NAME)
            .prefix("/backend")
            .sensitive_route(
                "/backend",
                "Role: admin required via AdminPlugin::require_role",
            );
        let report = run_conformance(&config, &routes);
        assert!(
            report.passed(),
            "AdminPlugin with custom prefix failed conformance:\n{}",
            report.to_text_report()
        );
    }

    #[test]
    fn admin_plugin_double_registration_detected() {
        // Simulate registering the admin plugin twice — its routes appear twice.
        let mut routes = admin_routes("/admin");
        routes.extend(admin_routes("/admin"));
        let result =
            autumn_web::plugin_conformance::check_duplicate_registration(PLUGIN_NAME, &routes);
        assert_eq!(
            result.status,
            autumn_web::plugin_conformance::CheckStatus::Fail,
            "expected duplicate-registration FAIL when plugin installed twice"
        );
    }

    #[test]
    fn admin_plugin_single_registration_passes_duplicate_check() {
        let routes = admin_routes("/admin");
        let result =
            autumn_web::plugin_conformance::check_duplicate_registration(PLUGIN_NAME, &routes);
        assert_eq!(
            result.status,
            autumn_web::plugin_conformance::CheckStatus::Pass,
            "single registration should pass: {}",
            result.message
        );
    }
}
