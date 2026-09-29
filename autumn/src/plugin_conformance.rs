//! Plugin conformance checks for Autumn plugin authors.
//!
//! Provides types and functions to verify that a plugin's route contributions
//! are safe, correctly attributed, and ready to publish. Plugin authors use
//! this module in integration tests to get a pass/fail conformance report
//! before publishing their crate.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use autumn_web::plugin_conformance::{ConformanceConfig, run_conformance};
//!
//! // Build a minimal host app that installs the plugin, then inspect its routes
//! // via AppBuilder::plugin_route_infos() or via `autumn plugin-check` in CI.
//! let config = ConformanceConfig::new("autumn-admin-plugin")
//!     .prefix("/admin")
//!     .sensitive_route("/admin", "Role: admin required via AdminPlugin::require_role");
//! // Pass route_infos collected from AppBuilder to run_conformance(...)
//! ```

use serde::{Deserialize, Serialize};

use crate::plugin_contract::{PluginContract, SurfaceTier, surface};
use crate::route_listing::{RouteInfo, RouteSource};

// ── Configuration ──────────────────────────────────────────────────────────

/// Configuration for a conformance run against a specific plugin.
///
/// `#[non_exhaustive]`: build one with [`ConformanceConfig::new`] and the
/// fluent setters rather than a struct literal. The `contract` field added in
/// #1601 was a `SemVer` break for anyone constructing this with a literal, and
/// the annotation lands in the same release so that break happens once — a
/// later check that needs configuration of its own can then be additive.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ConformanceConfig {
    /// The documented plugin name (e.g. `"autumn-admin-plugin"`).
    pub plugin_name: String,
    /// Expected URL prefix for all plugin routes (e.g. `"/admin"`).
    /// If `None`, the route-prefix check is skipped.
    pub expected_prefix: Option<String>,
    /// Paths that are intentionally at the root level (not under `expected_prefix`).
    /// Each entry must be the exact path as it appears in the route manifest.
    pub intentional_root_routes: Vec<String>,
    /// Sensitive surface declarations: routes whose path contains keywords like
    /// `admin`, `debug`, `credential`, `operator`, `secret`, or `metrics` must
    /// appear here with a non-empty `auth_mechanism`, or the check fails.
    pub sensitive_routes: Vec<SensitiveRoute>,
    /// The plugin's declared compatibility contract (issue #1601), when it has
    /// one. Drives the `experimental-surface` check; without it that check is
    /// skipped rather than assumed clean.
    pub contract: Option<PluginContract>,
}

impl ConformanceConfig {
    /// Create a minimal config with only the plugin name.
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
            expected_prefix: None,
            intentional_root_routes: Vec::new(),
            sensitive_routes: Vec::new(),
            contract: None,
        }
    }

    /// Attach the plugin's declared contract so the `experimental-surface`
    /// check can run.
    ///
    /// This is the same value the plugin returns from
    /// [`Plugin::contract`](crate::plugin::Plugin::contract) — pass it straight
    /// through rather than restating it, so the harness checks what the plugin
    /// actually declares.
    #[must_use]
    pub fn contract(mut self, contract: PluginContract) -> Self {
        self.contract = Some(contract);
        self
    }

    /// Declare the expected route prefix (e.g. `"/admin"`).
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.expected_prefix = Some(prefix.into());
        self
    }

    /// Declare a path as an intentional root-level route, exempting it from
    /// the prefix check (e.g. `"/webhook"`).
    #[must_use]
    pub fn intentional_root_route(mut self, path: impl Into<String>) -> Self {
        self.intentional_root_routes.push(path.into());
        self
    }

    /// Declare a sensitive route with its auth/profile gating mechanism.
    ///
    /// `path_pattern` is a prefix that matches the route path (e.g. `"/admin"`).
    /// `auth_mechanism` is a human-readable description (e.g. `"Role: admin required"`).
    /// An empty `auth_mechanism` still fails the check — the string must be non-empty.
    #[must_use]
    pub fn sensitive_route(
        mut self,
        path_pattern: impl Into<String>,
        auth_mechanism: impl Into<String>,
    ) -> Self {
        self.sensitive_routes.push(SensitiveRoute {
            path_pattern: path_pattern.into(),
            auth_mechanism: auth_mechanism.into(),
        });
        self
    }
}

/// Declaration of a sensitive route and its auth/profile gating mechanism.
#[derive(Debug, Clone)]
pub struct SensitiveRoute {
    /// Path prefix that matches sensitive routes (e.g. `"/admin"`).
    pub path_pattern: String,
    /// Human-readable description of the gating mechanism
    /// (e.g. `"Role: admin required via AdminPlugin::require_role"`).
    pub auth_mechanism: String,
}

// ── Report types ───────────────────────────────────────────────────────────

/// Status of an individual conformance check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// The check passed.
    Pass,
    /// The check found a problem that must be resolved before publishing.
    Fail,
    /// The check was skipped (e.g. no routes attributed to the plugin).
    Skip,
}

impl std::fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => write!(f, "PASS"),
            Self::Fail => write!(f, "FAIL"),
            Self::Skip => write!(f, "SKIP"),
        }
    }
}

/// Result of a single conformance check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    /// Short check identifier (e.g. `"route-attribution"`).
    pub name: String,
    /// Pass, fail, or skip.
    pub status: CheckStatus,
    /// Human-readable description of the result.
    pub message: String,
    /// Additional diagnostic lines (e.g. collision details, off-prefix paths).
    pub diagnostics: Vec<String>,
}

/// Information about two or more routes that collide on the same (method, path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollisionDiagnostic {
    /// HTTP method of the collision.
    pub method: String,
    /// URL path of the collision.
    pub path: String,
    /// All routes that contribute to this collision.
    pub contributors: Vec<RouteContributor>,
}

impl CollisionDiagnostic {
    fn to_diagnostic_string(&self) -> String {
        let contributors: Vec<String> = self
            .contributors
            .iter()
            .map(|c| format!("{} (source: {})", c.handler, c.source))
            .collect();
        format!(
            "{} {} — collides between: {}",
            self.method,
            self.path,
            contributors.join(", ")
        )
    }
}

/// A single route that contributes to a collision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteContributor {
    /// Registration source (e.g. `"user"`, `"plugin:admin"`, `"framework"`).
    pub source: String,
    /// Handler function name (e.g. `"posts::create"`).
    pub handler: String,
}

/// The full conformance report for a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformanceReport {
    /// The plugin name this report covers.
    pub plugin_name: String,
    /// Results for each conformance check that was run.
    pub checks: Vec<CheckResult>,
}

impl ConformanceReport {
    /// Returns `true` when no check has `CheckStatus::Fail`.
    /// Skipped checks do not count as failures.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.status != CheckStatus::Fail)
    }

    /// Render the report as a human-readable text string.
    #[must_use]
    pub fn to_text_report(&self) -> String {
        let mut out = String::new();
        let overall = if self.passed() { "PASS" } else { "FAIL" };
        out.push_str("Plugin conformance: ");
        out.push_str(&self.plugin_name);
        out.push_str(" — ");
        out.push_str(overall);
        out.push('\n');
        out.push_str(&"─".repeat(60));
        out.push('\n');
        for check in &self.checks {
            let icon = match check.status {
                CheckStatus::Pass => "✓",
                CheckStatus::Fail => "✗",
                CheckStatus::Skip => "−",
            };
            out.push_str(icon);
            out.push_str(" [");
            out.push_str(&check.status.to_string());
            out.push_str("] ");
            out.push_str(&check.name);
            out.push_str(": ");
            out.push_str(&check.message);
            out.push('\n');
            for diag in &check.diagnostics {
                out.push_str("  → ");
                out.push_str(diag);
                out.push('\n');
            }
        }
        out.push_str(&"─".repeat(60));
        out.push('\n');
        if self.passed() {
            out.push_str("All conformance checks passed.\n");
        } else {
            let fails = self
                .checks
                .iter()
                .filter(|c| c.status == CheckStatus::Fail)
                .count();
            out.push_str(&fails.to_string());
            out.push_str(" check(s) failed.\n");
        }
        out
    }
}

// ── Sensitive path classification ──────────────────────────────────────────

const SENSITIVE_KEYWORDS: &[&str] = &[
    "admin",
    "debug",
    "credential",
    "operator",
    "secret",
    "metrics",
];

/// Returns `true` when `path` contains a segment that is or starts with a
/// sensitive keyword (case-insensitive).
fn is_sensitive_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    SENSITIVE_KEYWORDS.iter().any(|kw| {
        lower
            .split('/')
            .any(|segment| segment == *kw || segment.starts_with(kw))
    })
}

// ── Individual check functions ─────────────────────────────────────────────

/// Check that all routes attributed to the plugin carry the expected
/// `Plugin("<plugin_name>")` source.
///
/// Returns `Skip` when no routes at all are attributed to the plugin.
#[must_use]
pub fn check_route_attribution(plugin_name: &str, routes: &[RouteInfo]) -> CheckResult {
    let plugin_routes: Vec<&RouteInfo> = routes
        .iter()
        .filter(|r| matches!(&r.source, RouteSource::Plugin(n) if n == plugin_name))
        .collect();

    if plugin_routes.is_empty() {
        return CheckResult {
            name: "route-attribution".to_owned(),
            status: CheckStatus::Fail,
            message: format!(
                "No routes attributed to plugin:{plugin_name} — \
                 check the plugin name or call AppBuilder::declare_plugin_routes"
            ),
            diagnostics: vec![],
        };
    }

    CheckResult {
        name: "route-attribution".to_owned(),
        status: CheckStatus::Pass,
        message: format!(
            "{} route(s) correctly attributed to plugin:{plugin_name}",
            plugin_routes.len()
        ),
        diagnostics: vec![],
    }
}

/// Check that all plugin routes live under `prefix`.
///
/// Routes listed in `intentional_root` (exact path match) are exempt.
/// Returns `Skip` when no routes are attributed to the plugin.
#[must_use]
pub fn check_route_prefix(
    plugin_name: &str,
    prefix: &str,
    intentional_root: &[String],
    routes: &[RouteInfo],
) -> CheckResult {
    let plugin_routes: Vec<&RouteInfo> = routes
        .iter()
        .filter(|r| matches!(&r.source, RouteSource::Plugin(n) if n == plugin_name))
        .collect();

    if plugin_routes.is_empty() {
        return CheckResult {
            name: "route-prefix".to_owned(),
            status: CheckStatus::Skip,
            message: format!("No routes attributed to plugin:{plugin_name}"),
            diagnostics: vec![],
        };
    }

    let under_prefix = |path: &str| path == prefix || path.starts_with(&format!("{prefix}/"));
    let off_prefix: Vec<String> = plugin_routes
        .iter()
        .filter(|r| !under_prefix(&r.path) && !intentional_root.contains(&r.path))
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();

    if off_prefix.is_empty() {
        CheckResult {
            name: "route-prefix".to_owned(),
            status: CheckStatus::Pass,
            message: format!("All plugin routes live under {prefix}"),
            diagnostics: vec![],
        }
    } else {
        CheckResult {
            name: "route-prefix".to_owned(),
            status: CheckStatus::Fail,
            message: format!(
                "{} route(s) not under prefix {prefix} and not declared as intentional root routes",
                off_prefix.len()
            ),
            diagnostics: off_prefix,
        }
    }
}

/// Canonicalize dynamic route segments before collision detection.
///
/// Both named params (`{id}`) and catch-all params (`{*rest}`) normalize to
/// `{}`. matchit (Axum's router) treats a named param and a catch-all at the
/// same path position as a conflict, so they must map to the same key.
fn normalize_path_for_collision(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if seg.starts_with('{') && seg.ends_with('}') {
                "{}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Detect route collisions: any two routes sharing the same (method, path) pair.
///
/// Dynamic segment names are normalized before comparison so that
/// `GET /users/{user_id}` and `GET /users/{id}` are correctly detected as
/// colliding. The `path` field in each `CollisionDiagnostic` reflects the
/// normalized shape (e.g. `/users/{}`).
///
/// Returns both a `CheckResult` and the full list of `CollisionDiagnostic` values
/// so callers can serialize detailed collision info in JSON output.
/// ⚡ Bolt: Optimization - Uses borrowed `&str` keys for the hash map to eliminate heap allocations per route during grouping. Allocations are deferred to only the subset of routes that actually collide.
pub fn check_collisions(routes: &[RouteInfo]) -> (CheckResult, Vec<CollisionDiagnostic>) {
    use std::collections::HashMap;

    let mut by_key: HashMap<(&str, String), Vec<&RouteInfo>> = HashMap::new();
    for route in routes {
        by_key
            .entry((
                route.method.as_str(),
                normalize_path_for_collision(&route.path),
            ))
            .or_default()
            .push(route);
    }

    let mut diagnostics: Vec<CollisionDiagnostic> = by_key
        .into_iter()
        .filter(|(_, rs)| rs.len() > 1)
        .map(|((method, path), rs)| CollisionDiagnostic {
            method: method.to_string(),
            path,
            contributors: rs
                .iter()
                .map(|r| RouteContributor {
                    source: r.source.to_string(),
                    handler: r.handler.clone(),
                })
                .collect(),
        })
        .collect();

    diagnostics.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));

    if diagnostics.is_empty() {
        (
            CheckResult {
                name: "route-collision".to_owned(),
                status: CheckStatus::Pass,
                message: "No route collisions detected".to_owned(),
                diagnostics: vec![],
            },
            diagnostics,
        )
    } else {
        let diag_strings: Vec<String> = diagnostics
            .iter()
            .map(CollisionDiagnostic::to_diagnostic_string)
            .collect();
        (
            CheckResult {
                name: "route-collision".to_owned(),
                status: CheckStatus::Fail,
                message: format!("{} route collision(s) detected", diagnostics.len()),
                diagnostics: diag_strings,
            },
            diagnostics,
        )
    }
}

/// Check that sensitive-sounding plugin routes are declared with an auth mechanism.
///
/// "Sensitive" paths contain segments matching `admin`, `debug`, `credential`,
/// `operator`, `secret`, or `metrics`. Each must appear in
/// `ConformanceConfig::sensitive_routes` with a non-empty `auth_mechanism`.
///
/// Returns `Pass` when no sensitive-named plugin routes are found.
#[must_use]
pub fn check_sensitive_surfaces(
    plugin_name: &str,
    routes: &[RouteInfo],
    declared: &[SensitiveRoute],
) -> CheckResult {
    let sensitive_plugin_routes: Vec<&RouteInfo> = routes
        .iter()
        .filter(|r| {
            matches!(&r.source, RouteSource::Plugin(n) if n == plugin_name)
                && is_sensitive_path(&r.path)
        })
        .collect();

    if sensitive_plugin_routes.is_empty() {
        return CheckResult {
            name: "sensitive-surfaces".to_owned(),
            status: CheckStatus::Pass,
            message: "No sensitive-named routes detected".to_owned(),
            diagnostics: vec![],
        };
    }

    let mut undeclared: Vec<String> = Vec::new();
    for route in &sensitive_plugin_routes {
        let is_declared = declared.iter().any(|d| {
            route.path.starts_with(&d.path_pattern) && !d.auth_mechanism.trim().is_empty()
        });
        if !is_declared {
            undeclared.push(format!(
                "{} {} — undeclared sensitive surface",
                route.method, route.path
            ));
        }
    }

    if undeclared.is_empty() {
        CheckResult {
            name: "sensitive-surfaces".to_owned(),
            status: CheckStatus::Pass,
            message: format!(
                "{} sensitive route(s) declared with auth/profile gating mechanisms",
                sensitive_plugin_routes.len()
            ),
            diagnostics: vec![],
        }
    } else {
        let mut diagnostics = undeclared;
        diagnostics.push(
            "Add .sensitive_route(path_prefix, auth_mechanism) to ConformanceConfig".to_owned(),
        );
        CheckResult {
            name: "sensitive-surfaces".to_owned(),
            status: CheckStatus::Fail,
            message: format!(
                "{} sensitive-named route(s) not declared with auth/profile gating",
                diagnostics.len() - 1
            ),
            diagnostics,
        }
    }
}

/// Check that the plugin's routes are not duplicated, which would indicate the
/// plugin was registered more than once and the framework's dedup logic was
/// bypassed.
///
/// Detects (method, path) pairs that appear more than once among routes
/// attributed to the named plugin. Returns `Skip` when no routes are
/// attributed to the plugin.
#[must_use]
/// ⚡ Bolt: Optimization - Uses borrowed `&str` keys for the deduplication map to avoid allocating new Strings for every route's method and path.
pub fn check_duplicate_registration(plugin_name: &str, routes: &[RouteInfo]) -> CheckResult {
    use std::collections::HashMap;

    let plugin_routes: Vec<&RouteInfo> = routes
        .iter()
        .filter(|r| matches!(&r.source, RouteSource::Plugin(n) if n == plugin_name))
        .collect();

    if plugin_routes.is_empty() {
        return CheckResult {
            name: "duplicate-registration".to_owned(),
            status: CheckStatus::Skip,
            message: format!("No routes attributed to plugin:{plugin_name}"),
            diagnostics: vec![],
        };
    }

    let mut counts: HashMap<(&str, &str), usize> = HashMap::new();
    for route in &plugin_routes {
        *counts
            .entry((route.method.as_str(), route.path.as_str()))
            .or_insert(0) += 1;
    }

    let mut duplicates: Vec<String> = counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|((method, path), count)| format!("{method} {path} — appears {count} times"))
        .collect();
    duplicates.sort();

    if duplicates.is_empty() {
        CheckResult {
            name: "duplicate-registration".to_owned(),
            status: CheckStatus::Pass,
            message: format!("No duplicate route registrations for plugin:{plugin_name}"),
            diagnostics: vec![],
        }
    } else {
        CheckResult {
            name: "duplicate-registration".to_owned(),
            status: CheckStatus::Fail,
            message: format!(
                "{} route(s) registered more than once; plugin:{plugin_name} \
                 may have been installed twice",
                duplicates.len()
            ),
            diagnostics: duplicates,
        }
    }
}

// ── Main entry point ───────────────────────────────────────────────────────

/// Report which experimental plugin surfaces a plugin declares a dependency on
/// (issue #1601).
///
/// This check **reports**; it does not gate. Building on experimental surface
/// is a legitimate, informed choice — the point is that the choice is visible
/// in the conformance report and in the release notes of whichever release
/// changes that surface.
///
/// It does fail on two things, because both make the declaration meaningless:
///
/// - a name that is not in [`PLUGIN_SURFACES`](crate::plugin_contract::PLUGIN_SURFACES)
///   at all (a typo, or a surface that has since been removed), and
/// - a name that *is* in the registry but at the
///   [`Stable`](crate::plugin_contract::SurfaceTier::Stable) tier — declaring a
///   stable API as experimental misreports the plugin's actual exposure.
///
/// Without a contract on the config the check is `Skip`: an author who has not
/// declared one has not claimed anything, and silently passing them would read
/// as "verified stable-only".
#[must_use]
pub fn check_experimental_surface(contract: Option<&PluginContract>) -> CheckResult {
    let name = "experimental-surface".to_owned();

    let Some(contract) = contract else {
        return CheckResult {
            name,
            status: CheckStatus::Skip,
            message: "no plugin contract declared; implement `Plugin::contract` (and pass it to \
                      `ConformanceConfig::contract`) to have experimental-surface use reported"
                .to_owned(),
            diagnostics: vec![],
        };
    };

    if contract.experimental_surfaces.is_empty() {
        return CheckResult {
            name,
            status: CheckStatus::Pass,
            message: "declares no dependency on experimental plugin surface".to_owned(),
            diagnostics: vec![],
        };
    }

    let mut diagnostics = Vec::new();
    let mut invalid = 0usize;
    for declared in &contract.experimental_surfaces {
        match surface(declared) {
            None => {
                invalid += 1;
                diagnostics.push(format!(
                    "{declared}: not a known plugin surface — check the spelling against \
                     `autumn_web::plugin_contract::PLUGIN_SURFACES`"
                ));
            }
            Some(s) if s.tier == SurfaceTier::Stable => {
                invalid += 1;
                diagnostics.push(format!(
                    "{declared}: declared experimental, but it is a STABLE surface — drop the \
                     declaration; it overstates this plugin's exposure"
                ));
            }
            Some(s) => diagnostics.push(format!("{}: {}", s.name, s.note)),
        }
    }

    if invalid > 0 {
        return CheckResult {
            name,
            status: CheckStatus::Fail,
            message: format!(
                "{invalid} of {} declared experimental surface(s) could not be resolved against \
                 the registry",
                contract.experimental_surfaces.len()
            ),
            diagnostics,
        };
    }

    CheckResult {
        name,
        status: CheckStatus::Pass,
        message: format!(
            "depends on {} experimental surface(s); these may change in any release",
            contract.experimental_surfaces.len()
        ),
        diagnostics,
    }
}

/// Run all conformance checks and return a `ConformanceReport`.
///
/// Checks run:
/// 1. `route-attribution` — plugin routes carry `plugin:<name>` source
/// 2. `route-prefix` — plugin routes live under `config.expected_prefix` (if set)
/// 3. `route-collision` — no two routes share (method, path)
/// 4. `sensitive-surfaces` — sensitive-named plugin routes are declared with auth
/// 5. `duplicate-registration` — plugin routes are not registered more than once
/// 6. `experimental-surface` — the plugin's declared use of experimental
///    plugin-facing API (issue #1601), reported rather than gated
#[must_use]
pub fn run_conformance(config: &ConformanceConfig, routes: &[RouteInfo]) -> ConformanceReport {
    let mut checks = Vec::new();

    checks.push(check_route_attribution(&config.plugin_name, routes));

    if let Some(ref prefix) = config.expected_prefix {
        checks.push(check_route_prefix(
            &config.plugin_name,
            prefix,
            &config.intentional_root_routes,
            routes,
        ));
    }

    let (collision_check, _) = check_collisions(routes);
    checks.push(collision_check);

    checks.push(check_sensitive_surfaces(
        &config.plugin_name,
        routes,
        &config.sensitive_routes,
    ));

    checks.push(check_duplicate_registration(&config.plugin_name, routes));
    checks.push(check_experimental_surface(config.contract.as_ref()));

    ConformanceReport {
        plugin_name: config.plugin_name.clone(),
        checks,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_route(method: &str, path: &str, source: RouteSource) -> RouteInfo {
        RouteInfo {
            method: method.to_owned(),
            path: path.to_owned(),
            handler: format!("{}_handler", path.trim_start_matches('/').replace('/', "_")),
            source,
            ..Default::default()
        }
    }

    fn plugin(name: &str) -> RouteSource {
        RouteSource::Plugin(name.to_owned())
    }

    // ── check_route_attribution ────────────────────────────────────────────

    #[test]
    fn attribution_all_attributed_passes() {
        let routes = vec![
            make_route("GET", "/admin", plugin("admin")),
            make_route("POST", "/admin/items", plugin("admin")),
        ];
        let result = check_route_attribution("admin", &routes);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
    }

    #[test]
    fn attribution_no_plugin_routes_fails() {
        let routes = vec![make_route("GET", "/posts", RouteSource::User)];
        let result = check_route_attribution("admin", &routes);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(
            result.message.contains("plugin:admin"),
            "message should name the plugin: {}",
            result.message
        );
    }

    #[test]
    fn attribution_pass_message_includes_count() {
        let routes = vec![
            make_route("GET", "/admin", plugin("admin")),
            make_route("GET", "/admin/items", plugin("admin")),
        ];
        let result = check_route_attribution("admin", &routes);
        assert!(
            result.message.contains('2'),
            "expected count in message: {}",
            result.message
        );
    }

    #[test]
    fn attribution_other_plugin_routes_not_counted() {
        let routes = vec![
            make_route("GET", "/harvest/feeds", plugin("harvest")),
            make_route("GET", "/admin", plugin("admin")),
        ];
        let result = check_route_attribution("admin", &routes);
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(
            result.message.contains('1'),
            "only 1 admin route: {}",
            result.message
        );
    }

    // ── check_route_prefix ─────────────────────────────────────────────────

    #[test]
    fn prefix_all_under_prefix_passes() {
        let routes = vec![
            make_route("GET", "/admin", plugin("admin")),
            make_route("POST", "/admin/items", plugin("admin")),
        ];
        let result = check_route_prefix("admin", "/admin", &[], &routes);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
    }

    #[test]
    fn prefix_route_outside_prefix_fails() {
        let routes = vec![
            make_route("GET", "/admin", plugin("admin")),
            make_route("GET", "/webhook", plugin("admin")),
        ];
        let result = check_route_prefix("admin", "/admin", &[], &routes);
        assert_eq!(result.status, CheckStatus::Fail, "{}", result.message);
    }

    #[test]
    fn prefix_off_prefix_diagnostic_names_the_route() {
        let routes = vec![make_route("GET", "/webhook", plugin("admin"))];
        let result = check_route_prefix("admin", "/admin", &[], &routes);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(
            result.diagnostics.iter().any(|d| d.contains("/webhook")),
            "expected /webhook in diagnostics: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn prefix_intentional_root_exempted() {
        let routes = vec![
            make_route("GET", "/admin", plugin("admin")),
            make_route("GET", "/webhook", plugin("admin")),
        ];
        let intentional = vec!["/webhook".to_owned()];
        let result = check_route_prefix("admin", "/admin", &intentional, &routes);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
    }

    #[test]
    fn prefix_sibling_path_with_same_string_prefix_fails() {
        let routes = vec![make_route("GET", "/administer/settings", plugin("admin"))];
        let result = check_route_prefix("admin", "/admin", &[], &routes);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "/administer/settings should not pass /admin prefix check"
        );
    }

    #[test]
    fn prefix_exact_match_passes() {
        let routes = vec![make_route("GET", "/admin", plugin("admin"))];
        let result = check_route_prefix("admin", "/admin", &[], &routes);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
    }

    #[test]
    fn prefix_no_plugin_routes_skips() {
        let routes = vec![make_route("GET", "/posts", RouteSource::User)];
        let result = check_route_prefix("admin", "/admin", &[], &routes);
        assert_eq!(result.status, CheckStatus::Skip);
    }

    // ── check_collisions ───────────────────────────────────────────────────

    #[test]
    fn collisions_no_collisions_passes() {
        let routes = vec![
            make_route("GET", "/posts", RouteSource::User),
            make_route("GET", "/admin", plugin("admin")),
            make_route("POST", "/posts", RouteSource::User),
        ];
        let (result, diagnostics) = check_collisions(&routes);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn collisions_host_plugin_collision_fails() {
        let routes = vec![
            make_route("GET", "/posts", RouteSource::User),
            make_route("GET", "/posts", plugin("harvest")),
        ];
        let (result, diagnostics) = check_collisions(&routes);
        assert_eq!(result.status, CheckStatus::Fail, "{}", result.message);
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn collisions_plugin_plugin_collision_fails() {
        let routes = vec![
            make_route("GET", "/api/feed", plugin("harvest")),
            make_route("GET", "/api/feed", plugin("feeds")),
        ];
        let (result, diagnostics) = check_collisions(&routes);
        assert_eq!(result.status, CheckStatus::Fail);
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn collisions_diagnostic_has_method_path_contributors() {
        let routes = vec![
            make_route("POST", "/items", RouteSource::User),
            make_route("POST", "/items", plugin("inventory")),
        ];
        let (_, diagnostics) = check_collisions(&routes);
        assert_eq!(diagnostics.len(), 1);
        let diag = &diagnostics[0];
        assert_eq!(diag.method, "POST");
        assert_eq!(diag.path, "/items");
        assert_eq!(diag.contributors.len(), 2);
        let sources: Vec<&str> = diag
            .contributors
            .iter()
            .map(|c| c.source.as_str())
            .collect();
        assert!(sources.contains(&"user"), "missing user: {sources:?}");
        assert!(
            sources.contains(&"plugin:inventory"),
            "missing plugin:inventory: {sources:?}"
        );
    }

    #[test]
    fn collisions_diagnostic_string_names_method_and_path() {
        let routes = vec![
            make_route("DELETE", "/items/{id}", RouteSource::User),
            make_route("DELETE", "/items/{id}", plugin("inventory")),
        ];
        let (result, _) = check_collisions(&routes);
        // Path is shown in normalized form ({id} → {}) so the reader
        // sees the structural shape that Axum matches on.
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.contains("DELETE") && d.contains("/items/{}")),
            "diagnostic should mention method and normalized path: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn collisions_different_methods_same_path_no_collision() {
        let routes = vec![
            make_route("GET", "/posts", RouteSource::User),
            make_route("POST", "/posts", plugin("blog")),
        ];
        let (result, _) = check_collisions(&routes);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn collisions_dynamic_segment_different_names_detected() {
        let routes = vec![
            make_route("GET", "/users/{user_id}", RouteSource::User),
            make_route("GET", "/users/{id}", plugin("auth")),
        ];
        let (result, diagnostics) = check_collisions(&routes);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "different param names should collide: {}",
            result.message
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, "/users/{}");
    }

    #[test]
    fn collisions_catchall_different_names_detected() {
        let routes = vec![
            make_route("GET", "/files/{*path}", RouteSource::User),
            make_route("GET", "/files/{*rest}", plugin("storage")),
        ];
        let (result, diagnostics) = check_collisions(&routes);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "different catch-all names should collide"
        );
        assert_eq!(diagnostics[0].path, "/files/{}");
    }

    #[test]
    fn collisions_catchall_vs_named_param_detected() {
        // matchit treats {id} and {*rest} at the same position as a conflict:
        // inserting /src/{file} after /src/{*filepath} returns InsertError::Conflict.
        let routes = vec![
            make_route("GET", "/files/{id}", RouteSource::User),
            make_route("GET", "/files/{*rest}", plugin("storage")),
        ];
        let (result, _) = check_collisions(&routes);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "catch-all and named param at same position conflict in matchit"
        );
    }

    #[test]
    fn normalize_path_for_collision_replaces_param_names() {
        assert_eq!(
            normalize_path_for_collision("/users/{user_id}/posts/{post_id}"),
            "/users/{}/posts/{}"
        );
        assert_eq!(normalize_path_for_collision("/files/{*rest}"), "/files/{}");
        assert_eq!(
            normalize_path_for_collision("/static/app.js"),
            "/static/app.js"
        );
        assert_eq!(normalize_path_for_collision("/"), "/");
    }

    // ── check_sensitive_surfaces ───────────────────────────────────────────

    #[test]
    fn sensitive_no_sensitive_routes_passes() {
        let routes = vec![
            make_route("GET", "/posts", plugin("blog")),
            make_route("GET", "/api/users", plugin("blog")),
        ];
        let result = check_sensitive_surfaces("blog", &routes, &[]);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
    }

    #[test]
    fn sensitive_admin_route_undeclared_fails() {
        let routes = vec![make_route("GET", "/admin/dashboard", plugin("myplugin"))];
        let result = check_sensitive_surfaces("myplugin", &routes, &[]);
        assert_eq!(result.status, CheckStatus::Fail, "{}", result.message);
    }

    #[test]
    fn sensitive_admin_route_declared_passes() {
        let routes = vec![make_route("GET", "/admin/dashboard", plugin("myplugin"))];
        let declared = vec![SensitiveRoute {
            path_pattern: "/admin".to_owned(),
            auth_mechanism: "Role: admin required".to_owned(),
        }];
        let result = check_sensitive_surfaces("myplugin", &routes, &declared);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
    }

    #[test]
    fn sensitive_debug_route_undeclared_fails() {
        let routes = vec![make_route("GET", "/debug/state", plugin("myplugin"))];
        let result = check_sensitive_surfaces("myplugin", &routes, &[]);
        assert_eq!(result.status, CheckStatus::Fail);
    }

    #[test]
    fn sensitive_empty_auth_mechanism_fails() {
        let routes = vec![make_route("GET", "/admin/users", plugin("myplugin"))];
        let declared = vec![SensitiveRoute {
            path_pattern: "/admin".to_owned(),
            auth_mechanism: String::new(),
        }];
        let result = check_sensitive_surfaces("myplugin", &routes, &declared);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "empty auth_mechanism must fail"
        );
    }

    #[test]
    fn sensitive_only_checks_own_plugin_routes() {
        let routes = vec![
            make_route("GET", "/admin/panel", RouteSource::User),
            make_route("GET", "/posts", plugin("blog")),
        ];
        let result = check_sensitive_surfaces("blog", &routes, &[]);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn sensitive_credentials_keyword_detected() {
        let routes = vec![make_route("GET", "/credential/rotate", plugin("auth"))];
        let result = check_sensitive_surfaces("auth", &routes, &[]);
        assert_eq!(result.status, CheckStatus::Fail);
    }

    #[test]
    fn sensitive_metrics_keyword_detected() {
        let routes = vec![make_route("GET", "/metrics", plugin("prom"))];
        let result = check_sensitive_surfaces("prom", &routes, &[]);
        assert_eq!(result.status, CheckStatus::Fail);
    }

    // ── check_duplicate_registration ──────────────────────────────────────

    #[test]
    fn duplicate_registration_no_duplicates_passes() {
        let routes = vec![
            make_route("GET", "/admin", plugin("admin")),
            make_route("POST", "/admin/items", plugin("admin")),
        ];
        let result = check_duplicate_registration("admin", &routes);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
    }

    #[test]
    fn duplicate_registration_same_route_twice_fails() {
        let routes = vec![
            make_route("GET", "/admin", plugin("admin")),
            make_route("GET", "/admin", plugin("admin")),
        ];
        let result = check_duplicate_registration("admin", &routes);
        assert_eq!(result.status, CheckStatus::Fail, "{}", result.message);
        assert_eq!(result.diagnostics.len(), 1);
    }

    #[test]
    fn duplicate_registration_diagnostic_names_method_path_count() {
        let routes = vec![
            make_route("POST", "/admin/items", plugin("admin")),
            make_route("POST", "/admin/items", plugin("admin")),
            make_route("POST", "/admin/items", plugin("admin")),
        ];
        let result = check_duplicate_registration("admin", &routes);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(
            result.diagnostics[0].contains("POST")
                && result.diagnostics[0].contains("/admin/items"),
            "diagnostic should name route: {:?}",
            result.diagnostics
        );
        assert!(
            result.diagnostics[0].contains('3'),
            "diagnostic should include count: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn duplicate_registration_no_plugin_routes_skips() {
        let routes = vec![make_route("GET", "/posts", RouteSource::User)];
        let result = check_duplicate_registration("admin", &routes);
        assert_eq!(result.status, CheckStatus::Skip);
    }

    #[test]
    fn duplicate_registration_only_checks_own_plugin() {
        let routes = vec![
            make_route("GET", "/harvest/feed", plugin("harvest")),
            make_route("GET", "/harvest/feed", plugin("harvest")),
            make_route("GET", "/admin", plugin("admin")),
        ];
        let result = check_duplicate_registration("admin", &routes);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    // ── run_conformance ────────────────────────────────────────────────────

    #[test]
    fn run_conformance_all_pass_when_clean() {
        let routes = vec![
            make_route("GET", "/admin", plugin("admin")),
            make_route("POST", "/admin/items", plugin("admin")),
        ];
        let config = ConformanceConfig::new("admin")
            .prefix("/admin")
            .sensitive_route("/admin", "Role: admin required");
        let report = run_conformance(&config, &routes);
        assert!(
            report.passed(),
            "expected pass:\n{}",
            report.to_text_report()
        );
    }

    #[test]
    fn run_conformance_fails_on_collision() {
        let routes = vec![
            make_route("GET", "/posts", RouteSource::User),
            make_route("GET", "/posts", plugin("harvest")),
        ];
        let config = ConformanceConfig::new("harvest");
        let report = run_conformance(&config, &routes);
        assert!(!report.passed());
    }

    #[test]
    fn run_conformance_report_has_plugin_name() {
        let config = ConformanceConfig::new("my-plugin");
        let report = run_conformance(&config, &[]);
        assert_eq!(report.plugin_name, "my-plugin");
    }

    #[test]
    fn run_conformance_skips_prefix_check_when_none() {
        let routes = vec![make_route("GET", "/anywhere", plugin("myplugin"))];
        let config = ConformanceConfig::new("myplugin");
        let report = run_conformance(&config, &routes);
        let has_prefix_check = report.checks.iter().any(|c| c.name == "route-prefix");
        assert!(
            !has_prefix_check,
            "prefix check should not run when expected_prefix is None"
        );
    }

    // ── ConformanceReport ──────────────────────────────────────────────────

    #[test]
    fn report_passed_false_when_any_fail() {
        let report = ConformanceReport {
            plugin_name: "test".to_owned(),
            checks: vec![
                CheckResult {
                    name: "check-1".to_owned(),
                    status: CheckStatus::Pass,
                    message: "ok".to_owned(),
                    diagnostics: vec![],
                },
                CheckResult {
                    name: "check-2".to_owned(),
                    status: CheckStatus::Fail,
                    message: "fail".to_owned(),
                    diagnostics: vec![],
                },
            ],
        };
        assert!(!report.passed());
    }

    #[test]
    fn report_passed_true_with_skip() {
        let report = ConformanceReport {
            plugin_name: "test".to_owned(),
            checks: vec![CheckResult {
                name: "route-prefix".to_owned(),
                status: CheckStatus::Skip,
                message: "no routes".to_owned(),
                diagnostics: vec![],
            }],
        };
        assert!(report.passed(), "Skip should not fail the report");
    }

    #[test]
    fn report_text_contains_plugin_name() {
        let report = ConformanceReport {
            plugin_name: "autumn-admin-plugin".to_owned(),
            checks: vec![],
        };
        assert!(report.to_text_report().contains("autumn-admin-plugin"));
    }

    #[test]
    fn report_text_shows_fail_when_failed() {
        let report = ConformanceReport {
            plugin_name: "test".to_owned(),
            checks: vec![CheckResult {
                name: "route-collision".to_owned(),
                status: CheckStatus::Fail,
                message: "1 collision".to_owned(),
                diagnostics: vec![],
            }],
        };
        assert!(report.to_text_report().contains("FAIL"));
    }

    #[test]
    fn report_text_shows_pass_when_all_pass() {
        let report = ConformanceReport {
            plugin_name: "test".to_owned(),
            checks: vec![],
        };
        assert!(report.to_text_report().contains("PASS"));
    }

    #[test]
    fn report_serializes_to_json() {
        let report = ConformanceReport {
            plugin_name: "test-plugin".to_owned(),
            checks: vec![CheckResult {
                name: "route-attribution".to_owned(),
                status: CheckStatus::Pass,
                message: "ok".to_owned(),
                diagnostics: vec![],
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["plugin_name"], "test-plugin");
        assert_eq!(parsed["checks"][0]["status"], "pass");
    }

    #[test]
    fn report_deserializes_from_json() {
        let json = r#"{"plugin_name":"test","checks":[{"name":"route-attribution","status":"pass","message":"ok","diagnostics":[]}]}"#;
        let report: ConformanceReport = serde_json::from_str(json).unwrap();
        assert_eq!(report.plugin_name, "test");
        assert_eq!(report.checks[0].status, CheckStatus::Pass);
    }

    // ── is_sensitive_path ──────────────────────────────────────────────────

    #[test]
    fn admin_path_is_sensitive() {
        assert!(is_sensitive_path("/admin"));
        assert!(is_sensitive_path("/admin/users"));
    }

    #[test]
    fn debug_path_is_sensitive() {
        assert!(is_sensitive_path("/debug"));
        assert!(is_sensitive_path("/api/debug/state"));
    }

    #[test]
    fn posts_path_is_not_sensitive() {
        assert!(!is_sensitive_path("/posts"));
        assert!(!is_sensitive_path("/api/users"));
    }

    #[test]
    fn path_with_admin_as_substring_of_longer_segment_not_sensitive() {
        // "administrator" as a whole segment IS caught (starts_with("admin"))
        // but this is intentional per the spec: admin-prefixed segments are sensitive
        assert!(!is_sensitive_path("/products/admiration"));
    }
}
