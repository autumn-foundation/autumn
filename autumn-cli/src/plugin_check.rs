//! `autumn plugin-check` — run conformance checks against a plugin's routes.
//!
//! Compiles the target binary (debug profile), runs it with
//! `AUTUMN_DUMP_ROUTES=1` to collect the route manifest, then applies
//! five conformance checks and outputs a pass/fail report.
//!
//! # Checks
//!
//! | Check | Description |
//! |-------|-------------|
//! | `installability` | Binary compiled and route manifest collected |
//! | `route-attribution` | Plugin routes carry `plugin:<name>` source |
//! | `route-prefix` | Plugin routes live under the declared prefix |
//! | `route-collision` | No two routes share (method, path) |
//! | `sensitive-surfaces` | Sensitive-named routes declared with auth info |

use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::routes::{RouteInfo, compile_binary, find_binary};

// ── Report types ───────────────────────────────────────────────────────────

/// Status of a conformance check item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Fail,
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
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
    pub diagnostics: Vec<String>,
}

/// Full conformance report for a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformanceReport {
    pub plugin_name: String,
    pub checks: Vec<CheckResult>,
}

impl ConformanceReport {
    /// Returns `true` when no check has `CheckStatus::Fail`.
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.status != CheckStatus::Fail)
    }

    /// Render the report as human-readable text.
    pub fn to_text_report(&self) -> String {
        let mut out = String::new();
        let overall = if self.passed() { "PASS" } else { "FAIL" };
        out.push_str("Plugin conformance: ");
        out.push_str(&self.plugin_name);
        out.push_str(" \u{2014} ");
        out.push_str(overall);
        out.push('\n');
        out.push_str(&"\u{2500}".repeat(60));
        out.push('\n');
        for check in &self.checks {
            let icon = match check.status {
                CheckStatus::Pass => "\u{2713}",
                CheckStatus::Fail => "\u{2717}",
                CheckStatus::Skip => "\u{2212}",
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
                out.push_str("  \u{2192} ");
                out.push_str(diag);
                out.push('\n');
            }
        }
        out.push_str(&"\u{2500}".repeat(60));
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

/// Output format for the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportFormat {
    Text,
    Json,
}

impl std::str::FromStr for ReportFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "text" | "table" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "unknown format '{other}'; expected 'text' or 'json'"
            )),
        }
    }
}

/// A declared sensitive route with its auth/profile gating description.
#[derive(Debug, Clone)]
pub struct SensitiveRouteDecl {
    /// Path prefix (e.g. `"/admin"`).
    pub path_pattern: String,
    /// Human-readable auth description (e.g. `"Role: admin required"`).
    pub auth_mechanism: String,
}

/// Options for `autumn plugin-check`.
pub struct PluginCheckOptions<'a> {
    /// Cargo package to build (workspace multi-package projects).
    pub package: Option<&'a str>,
    /// Binary target name (for packages with multiple `[[bin]]` targets).
    pub bin: Option<&'a str>,
    /// The documented plugin name to check (e.g. `"autumn-admin-plugin"`).
    pub plugin_name: &'a str,
    /// Expected URL prefix for plugin routes (e.g. `"/admin"`).
    pub expected_prefix: Option<&'a str>,
    /// Declared sensitive routes with their auth mechanisms.
    pub sensitive_routes: &'a [SensitiveRouteDecl],
    /// Output format.
    pub format: ReportFormat,
}

/// Run `autumn plugin-check`.
pub fn run(opts: &PluginCheckOptions<'_>) {
    eprintln!("\u{1F342} autumn plugin-check\n");
    compile_binary(opts.package, opts.bin);
    let binary = find_binary(opts.package, opts.bin);

    let output = Command::new(&binary)
        .env("AUTUMN_DUMP_ROUTES", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    if !output.status.success() {
        eprintln!(
            "\u{2717} Binary exited with status {} while dumping routes",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let routes: Vec<RouteInfo> = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        eprintln!("\u{2717} Failed to parse route listing JSON: {e}");
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    });

    let report = build_report(opts, &routes);

    match opts.format {
        ReportFormat::Text => print!("{}", report.to_text_report()),
        ReportFormat::Json => {
            let json = serde_json::to_string_pretty(&report)
                .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"));
            println!("{json}");
        }
    }

    if !report.passed() {
        std::process::exit(1);
    }
}

/// Build the conformance report from a route listing and options.
///
/// Public so callers can unit-test the analysis without running a binary.
pub fn build_report(opts: &PluginCheckOptions<'_>, routes: &[RouteInfo]) -> ConformanceReport {
    let mut checks = Vec::new();

    checks.push(CheckResult {
        name: "installability".to_owned(),
        status: CheckStatus::Pass,
        message: format!("{} routes collected from binary", routes.len()),
        diagnostics: vec![],
    });

    checks.push(check_route_attribution(opts.plugin_name, routes));

    if let Some(prefix) = opts.expected_prefix {
        checks.push(check_route_prefix(opts.plugin_name, prefix, routes));
    }

    checks.push(check_collisions(routes));
    checks.push(check_sensitive_surfaces(
        opts.plugin_name,
        routes,
        opts.sensitive_routes,
    ));
    checks.push(check_duplicate_registration(opts.plugin_name, routes));

    ConformanceReport {
        plugin_name: opts.plugin_name.to_owned(),
        checks,
    }
}

// ── Individual check helpers ───────────────────────────────────────────────

fn check_route_attribution(plugin_name: &str, routes: &[RouteInfo]) -> CheckResult {
    let expected = format!("plugin:{plugin_name}");
    let plugin_routes: Vec<&RouteInfo> = routes.iter().filter(|r| r.source == expected).collect();

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

fn check_route_prefix(plugin_name: &str, prefix: &str, routes: &[RouteInfo]) -> CheckResult {
    let expected = format!("plugin:{plugin_name}");
    let plugin_routes: Vec<&RouteInfo> = routes.iter().filter(|r| r.source == expected).collect();

    if plugin_routes.is_empty() {
        return CheckResult {
            name: "route-prefix".to_owned(),
            status: CheckStatus::Skip,
            message: format!("No routes attributed to plugin:{plugin_name}"),
            diagnostics: vec![],
        };
    }

    let off_prefix: Vec<String> = plugin_routes
        .iter()
        .filter(|r| {
            let p = &r.path;
            p != prefix && !p.starts_with(&format!("{prefix}/"))
        })
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
            message: format!("{} route(s) not under prefix {prefix}", off_prefix.len()),
            diagnostics: off_prefix,
        }
    }
}

const SENSITIVE_KEYWORDS: &[&str] = &[
    "admin",
    "debug",
    "credential",
    "operator",
    "secret",
    "metrics",
];

fn is_sensitive_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    SENSITIVE_KEYWORDS.iter().any(|kw| {
        lower
            .split('/')
            .any(|segment| segment == *kw || segment.starts_with(kw))
    })
}

fn check_sensitive_surfaces(
    plugin_name: &str,
    routes: &[RouteInfo],
    declared: &[SensitiveRouteDecl],
) -> CheckResult {
    let expected = format!("plugin:{plugin_name}");
    let sensitive: Vec<&RouteInfo> = routes
        .iter()
        .filter(|r| r.source == expected && is_sensitive_path(&r.path))
        .collect();

    if sensitive.is_empty() {
        return CheckResult {
            name: "sensitive-surfaces".to_owned(),
            status: CheckStatus::Pass,
            message: "No sensitive-named routes detected".to_owned(),
            diagnostics: vec![],
        };
    }

    let mut undeclared: Vec<String> = Vec::new();
    for route in &sensitive {
        let is_ok = declared.iter().any(|d| {
            route.path.starts_with(&d.path_pattern) && !d.auth_mechanism.trim().is_empty()
        });
        if !is_ok {
            undeclared.push(format!(
                "{} {} \u{2014} document auth/profile gating with --sensitive-route",
                route.method, route.path
            ));
        }
    }

    if undeclared.is_empty() {
        CheckResult {
            name: "sensitive-surfaces".to_owned(),
            status: CheckStatus::Pass,
            message: format!(
                "{} sensitive route(s) declared with auth/profile gating",
                sensitive.len()
            ),
            diagnostics: vec![],
        }
    } else {
        CheckResult {
            name: "sensitive-surfaces".to_owned(),
            status: CheckStatus::Fail,
            message: format!(
                "{} sensitive-named route(s) not declared with auth/profile gating",
                undeclared.len()
            ),
            diagnostics: undeclared,
        }
    }
}

fn check_duplicate_registration(plugin_name: &str, routes: &[RouteInfo]) -> CheckResult {
    use std::collections::HashMap;

    let expected = format!("plugin:{plugin_name}");
    let plugin_routes: Vec<&RouteInfo> = routes.iter().filter(|r| r.source == expected).collect();

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
        *counts.entry((&route.method, &route.path)).or_insert(0) += 1;
    }

    let mut duplicates: Vec<String> = counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|((method, path), count)| format!("{method} {path} \u{2014} appears {count} times"))
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

fn check_collisions(routes: &[RouteInfo]) -> CheckResult {
    use std::collections::HashMap;

    let mut by_key: HashMap<(String, String), Vec<&RouteInfo>> = HashMap::new();
    for route in routes {
        by_key
            .entry((
                route.method.clone(),
                normalize_path_for_collision(&route.path),
            ))
            .or_default()
            .push(route);
    }

    let mut collisions: Vec<String> = by_key
        .iter()
        .filter(|(_, rs)| rs.len() > 1)
        .map(|((method, path), rs)| {
            let contributors: Vec<String> = rs
                .iter()
                .map(|r| format!("{} ({})", r.handler, r.source))
                .collect();
            format!(
                "{method} {path} \u{2014} collides between: {}",
                contributors.join(", ")
            )
        })
        .collect();
    collisions.sort();

    if collisions.is_empty() {
        CheckResult {
            name: "route-collision".to_owned(),
            status: CheckStatus::Pass,
            message: "No route collisions detected".to_owned(),
            diagnostics: vec![],
        }
    } else {
        CheckResult {
            name: "route-collision".to_owned(),
            status: CheckStatus::Fail,
            message: format!("{} route collision(s) detected", collisions.len()),
            diagnostics: collisions,
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_route(method: &str, path: &str, source: &str) -> RouteInfo {
        RouteInfo {
            method: method.to_owned(),
            path: path.to_owned(),
            handler: format!("{}_handler", path.trim_start_matches('/').replace('/', "_")),
            source: source.to_owned(),
            middleware: vec![],
            api_version: None,
            status: None,
            sunset_opt_out: None,
            resource_shape: String::new(),
            pools: Vec::new(),
        }
    }

    fn no_sensitive() -> Vec<SensitiveRouteDecl> {
        vec![]
    }

    // ── check_route_attribution ────────────────────────────────────────────

    #[test]
    fn attribution_all_attributed_passes() {
        let routes = vec![
            make_route("GET", "/admin", "plugin:admin"),
            make_route("POST", "/admin/items", "plugin:admin"),
        ];
        let result = check_route_attribution("admin", &routes);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
    }

    #[test]
    fn attribution_no_plugin_routes_fails() {
        let routes = vec![make_route("GET", "/posts", "user")];
        let result = check_route_attribution("admin", &routes);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(
            result.message.contains("plugin:admin"),
            "message should name the plugin: {}",
            result.message
        );
    }

    #[test]
    fn attribution_message_includes_count() {
        let routes = vec![
            make_route("GET", "/admin", "plugin:admin"),
            make_route("GET", "/admin/items", "plugin:admin"),
        ];
        let result = check_route_attribution("admin", &routes);
        assert!(result.message.contains('2'), "{}", result.message);
    }

    // ── check_route_prefix ─────────────────────────────────────────────────

    #[test]
    fn prefix_all_under_prefix_passes() {
        let routes = vec![
            make_route("GET", "/admin", "plugin:admin"),
            make_route("POST", "/admin/items", "plugin:admin"),
        ];
        let result = check_route_prefix("admin", "/admin", &routes);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
    }

    #[test]
    fn prefix_route_outside_fails_with_diagnostic() {
        let routes = vec![
            make_route("GET", "/admin", "plugin:admin"),
            make_route("GET", "/webhook", "plugin:admin"),
        ];
        let result = check_route_prefix("admin", "/admin", &routes);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.diagnostics.iter().any(|d| d.contains("/webhook")));
    }

    #[test]
    fn prefix_no_plugin_routes_skips() {
        let routes = vec![make_route("GET", "/posts", "user")];
        let result = check_route_prefix("admin", "/admin", &routes);
        assert_eq!(result.status, CheckStatus::Skip);
    }

    // ── check_collisions ───────────────────────────────────────────────────

    #[test]
    fn collisions_no_collisions_passes() {
        let routes = vec![
            make_route("GET", "/posts", "user"),
            make_route("GET", "/admin", "plugin:admin"),
        ];
        let result = check_collisions(&routes);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn collisions_host_plugin_collision_fails() {
        let routes = vec![
            make_route("GET", "/posts", "user"),
            make_route("GET", "/posts", "plugin:harvest"),
        ];
        let result = check_collisions(&routes);
        assert_eq!(result.status, CheckStatus::Fail);
        assert_eq!(result.diagnostics.len(), 1);
    }

    #[test]
    fn collisions_plugin_plugin_collision_fails() {
        let routes = vec![
            make_route("GET", "/api/feed", "plugin:harvest"),
            make_route("GET", "/api/feed", "plugin:feeds"),
        ];
        let result = check_collisions(&routes);
        assert_eq!(result.status, CheckStatus::Fail);
    }

    #[test]
    fn collisions_diagnostic_names_method_path_contributors() {
        let routes = vec![
            make_route("POST", "/items", "user"),
            make_route("POST", "/items", "plugin:inventory"),
        ];
        let result = check_collisions(&routes);
        assert_eq!(result.status, CheckStatus::Fail);
        let diag = &result.diagnostics[0];
        assert!(diag.contains("POST"), "missing method: {diag}");
        assert!(diag.contains("/items"), "missing path: {diag}");
        assert!(diag.contains("user"), "missing user: {diag}");
        assert!(diag.contains("plugin:inventory"), "missing plugin: {diag}");
    }

    #[test]
    fn collisions_different_methods_no_collision() {
        let routes = vec![
            make_route("GET", "/posts", "user"),
            make_route("POST", "/posts", "plugin:blog"),
        ];
        let result = check_collisions(&routes);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn collisions_dynamic_segment_different_names_detected() {
        let routes = vec![
            make_route("GET", "/users/{user_id}", "user"),
            make_route("GET", "/users/{id}", "plugin:auth"),
        ];
        let result = check_collisions(&routes);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "different param names should collide: {}",
            result.message
        );
    }

    #[test]
    fn collisions_catchall_different_names_detected() {
        let routes = vec![
            make_route("GET", "/files/{*path}", "user"),
            make_route("GET", "/files/{*rest}", "plugin:storage"),
        ];
        let result = check_collisions(&routes);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "different catch-all names should collide"
        );
    }

    #[test]
    fn collisions_catchall_vs_named_param_detected() {
        // matchit treats {id} and {*rest} at the same position as a conflict.
        let routes = vec![
            make_route("GET", "/files/{id}", "user"),
            make_route("GET", "/files/{*rest}", "plugin:storage"),
        ];
        let result = check_collisions(&routes);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "catch-all and named param at same position conflict in matchit"
        );
    }

    #[test]
    fn normalize_path_replaces_param_names() {
        assert_eq!(
            normalize_path_for_collision("/users/{user_id}/posts/{post_id}"),
            "/users/{}/posts/{}"
        );
        assert_eq!(normalize_path_for_collision("/files/{*rest}"), "/files/{}");
        assert_eq!(
            normalize_path_for_collision("/static/app.js"),
            "/static/app.js"
        );
    }

    // ── check_sensitive_surfaces ───────────────────────────────────────────

    #[test]
    fn sensitive_no_sensitive_routes_passes() {
        let routes = vec![
            make_route("GET", "/posts", "plugin:blog"),
            make_route("GET", "/api/users", "plugin:blog"),
        ];
        let result = check_sensitive_surfaces("blog", &routes, &no_sensitive());
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn sensitive_admin_route_undeclared_fails() {
        let routes = vec![make_route("GET", "/admin/dashboard", "plugin:myplugin")];
        let result = check_sensitive_surfaces("myplugin", &routes, &no_sensitive());
        assert_eq!(result.status, CheckStatus::Fail);
    }

    #[test]
    fn sensitive_admin_route_declared_passes() {
        let routes = vec![make_route("GET", "/admin/dashboard", "plugin:myplugin")];
        let declared = vec![SensitiveRouteDecl {
            path_pattern: "/admin".to_owned(),
            auth_mechanism: "Role: admin required".to_owned(),
        }];
        let result = check_sensitive_surfaces("myplugin", &routes, &declared);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
    }

    #[test]
    fn sensitive_empty_auth_mechanism_fails() {
        let routes = vec![make_route("GET", "/admin/users", "plugin:myplugin")];
        let declared = vec![SensitiveRouteDecl {
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
    fn sensitive_only_checks_plugin_routes() {
        let routes = vec![
            make_route("GET", "/admin/panel", "user"),
            make_route("GET", "/posts", "plugin:blog"),
        ];
        let result = check_sensitive_surfaces("blog", &routes, &no_sensitive());
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn sensitive_debug_route_fails() {
        let routes = vec![make_route("GET", "/debug/state", "plugin:myplugin")];
        let result = check_sensitive_surfaces("myplugin", &routes, &no_sensitive());
        assert_eq!(result.status, CheckStatus::Fail);
    }

    // ── ConformanceReport ──────────────────────────────────────────────────

    #[test]
    fn report_passed_true_all_pass() {
        let report = ConformanceReport {
            plugin_name: "test".to_owned(),
            checks: vec![
                CheckResult {
                    name: "c1".to_owned(),
                    status: CheckStatus::Pass,
                    message: "ok".to_owned(),
                    diagnostics: vec![],
                },
                CheckResult {
                    name: "c2".to_owned(),
                    status: CheckStatus::Skip,
                    message: "skipped".to_owned(),
                    diagnostics: vec![],
                },
            ],
        };
        assert!(report.passed());
    }

    #[test]
    fn report_passed_false_any_fail() {
        let report = ConformanceReport {
            plugin_name: "test".to_owned(),
            checks: vec![CheckResult {
                name: "c1".to_owned(),
                status: CheckStatus::Fail,
                message: "fail".to_owned(),
                diagnostics: vec![],
            }],
        };
        assert!(!report.passed());
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
    fn report_text_shows_overall_pass() {
        let report = ConformanceReport {
            plugin_name: "test".to_owned(),
            checks: vec![],
        };
        assert!(report.to_text_report().contains("PASS"));
    }

    #[test]
    fn report_text_shows_overall_fail() {
        let report = ConformanceReport {
            plugin_name: "test".to_owned(),
            checks: vec![CheckResult {
                name: "c1".to_owned(),
                status: CheckStatus::Fail,
                message: "fail".to_owned(),
                diagnostics: vec![],
            }],
        };
        assert!(report.to_text_report().contains("FAIL"));
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

    // ── ReportFormat parsing ───────────────────────────────────────────────

    #[test]
    fn parse_format_text() {
        let f: ReportFormat = "text".parse().unwrap();
        assert_eq!(f, ReportFormat::Text);
    }

    #[test]
    fn parse_format_table_alias() {
        let f: ReportFormat = "table".parse().unwrap();
        assert_eq!(f, ReportFormat::Text);
    }

    #[test]
    fn parse_format_json() {
        let f: ReportFormat = "json".parse().unwrap();
        assert_eq!(f, ReportFormat::Json);
    }

    #[test]
    fn parse_format_unknown_is_error() {
        let r: Result<ReportFormat, _> = "xml".parse();
        assert!(r.is_err());
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
    fn normal_paths_not_sensitive() {
        assert!(!is_sensitive_path("/posts"));
        assert!(!is_sensitive_path("/api/users"));
    }

    // ── check_duplicate_registration ──────────────────────────────────────

    #[test]
    fn duplicate_no_duplicates_passes() {
        let routes = vec![
            make_route("GET", "/admin", "plugin:admin"),
            make_route("POST", "/admin/items", "plugin:admin"),
        ];
        let result = check_duplicate_registration("admin", &routes);
        assert_eq!(result.status, CheckStatus::Pass, "{}", result.message);
    }

    #[test]
    fn duplicate_same_route_twice_fails() {
        let routes = vec![
            make_route("GET", "/admin", "plugin:admin"),
            make_route("GET", "/admin", "plugin:admin"),
        ];
        let result = check_duplicate_registration("admin", &routes);
        assert_eq!(result.status, CheckStatus::Fail);
        assert_eq!(result.diagnostics.len(), 1);
    }

    #[test]
    fn duplicate_diagnostic_names_route() {
        let routes = vec![
            make_route("POST", "/admin/items", "plugin:admin"),
            make_route("POST", "/admin/items", "plugin:admin"),
        ];
        let result = check_duplicate_registration("admin", &routes);
        assert_eq!(result.status, CheckStatus::Fail);
        let diag = &result.diagnostics[0];
        assert!(diag.contains("POST"), "missing method: {diag}");
        assert!(diag.contains("/admin/items"), "missing path: {diag}");
    }

    #[test]
    fn duplicate_no_plugin_routes_skips() {
        let routes = vec![make_route("GET", "/posts", "user")];
        let result = check_duplicate_registration("admin", &routes);
        assert_eq!(result.status, CheckStatus::Skip);
    }

    // ── build_report ───────────────────────────────────────────────────────

    #[test]
    fn build_report_includes_installability_check() {
        let opts = PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "test",
            expected_prefix: None,
            sensitive_routes: &[],
            format: ReportFormat::Text,
        };
        let routes = vec![make_route("GET", "/posts", "user")];
        let report = build_report(&opts, &routes);
        assert!(report.checks.iter().any(|c| c.name == "installability"));
    }

    #[test]
    fn build_report_skips_prefix_check_when_none() {
        let opts = PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "test",
            expected_prefix: None,
            sensitive_routes: &[],
            format: ReportFormat::Text,
        };
        let report = build_report(&opts, &[]);
        assert!(!report.checks.iter().any(|c| c.name == "route-prefix"));
    }

    #[test]
    fn build_report_includes_prefix_check_when_set() {
        let opts = PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "admin",
            expected_prefix: Some("/admin"),
            sensitive_routes: &[],
            format: ReportFormat::Text,
        };
        let routes = vec![make_route("GET", "/admin", "plugin:admin")];
        let report = build_report(&opts, &routes);
        assert!(report.checks.iter().any(|c| c.name == "route-prefix"));
    }

    #[test]
    fn build_report_plugin_name_in_report() {
        let opts = PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "autumn-admin-plugin",
            expected_prefix: None,
            sensitive_routes: &[],
            format: ReportFormat::Text,
        };
        let report = build_report(&opts, &[]);
        assert_eq!(report.plugin_name, "autumn-admin-plugin");
    }
}
