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
//! | `plugin-contract` | The plugin declares a parseable `autumn-web` range |
//! | `experimental-surface` | The plugin's declared use of experimental API |
//!
//! The last two read the contract the built binary dumps after
//! [`PLUGIN_CONTRACT_MARKER`](autumn_web::plugin_contract::PLUGIN_CONTRACT_MARKER)
//! (issue #1601). A binary that emits no marker — an app built before the
//! contract existed — skips both rather than failing.

use std::process::Command;

use serde::{Deserialize, Serialize};

use autumn_web::plugin_contract::{
    ContractVerdict, PLUGIN_CONTRACT_MARKER, PluginContract, SurfaceTier, evaluate, surface,
};

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
    /// What the built binary's stderr said about its plugin contracts
    /// (issue #1601). See [`ContractDump`].
    pub contracts: &'a ContractDump,
    /// Turn the `experimental-surface` report into a hard failure.
    ///
    /// Off by default: depending on experimental surface is an informed choice,
    /// not a defect. A plugin whose own CI wants to forbid it passes
    /// `--deny-experimental`.
    pub deny_experimental: bool,
}

/// Run `autumn plugin-check`.
pub fn run(opts: &PluginCheckOptions<'_>) {
    eprintln!("\u{1F342} autumn plugin-check\n");
    compile_binary(opts.package, opts.bin);
    let binary = find_binary(opts.package, opts.bin);

    // stderr is PIPED rather than inherited so the plugin-contract marker can be
    // read off it (issue #1601). Everything the child wrote is relayed verbatim
    // afterwards, minus the marker line itself, so the operator still sees the
    // binary's own diagnostics.
    let output = Command::new(&binary)
        .env("AUTUMN_DUMP_ROUTES", "1")
        .env("AUTUMN_DUMP_PLUGIN_CONTRACT", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        if !line.starts_with(PLUGIN_CONTRACT_MARKER) {
            eprintln!("{line}");
        }
    }
    let contracts = parse_plugin_contracts(&stderr);

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

    let report = build_report(
        &PluginCheckOptions {
            package: opts.package,
            bin: opts.bin,
            plugin_name: opts.plugin_name,
            expected_prefix: opts.expected_prefix,
            sensitive_routes: opts.sensitive_routes,
            format: opts.format.clone(),
            contracts: &contracts,
            deny_experimental: opts.deny_experimental,
        },
        &routes,
    );

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
        // Deliberately source-neutral: `autumn plugin-check` collects these by
        // running a binary, `autumn plugin inspect` reads them out of a
        // sandboxed plugin's manifest with no binary in sight.
        message: format!(
            "{count} route{plural} collected",
            count = routes.len(),
            plural = if routes.len() == 1 { "" } else { "s" }
        ),
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

    let declared = match opts.contracts {
        ContractDump::Present(all) => find_declared(all, opts.plugin_name),
        ContractDump::Absent | ContractDump::Malformed(_) => None,
    };
    checks.push(check_plugin_contract(
        opts.plugin_name,
        opts.contracts,
        declared,
    ));
    checks.push(check_experimental_surface(
        opts.contracts,
        declared,
        opts.deny_experimental,
    ));

    ConformanceReport {
        plugin_name: opts.plugin_name.to_owned(),
        checks,
    }
}

/// What the child binary's stderr said about its plugin contracts.
///
/// Three outcomes, deliberately kept apart. Folding the third into the first
/// would let `autumn plugin-check` report a green run for a plugin it never
/// actually checked, which is the one thing an author gate must not do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractDump {
    /// No [`PLUGIN_CONTRACT_MARKER`] line at all — an application built against
    /// an `autumn-web` that predates the contract.
    Absent,
    /// The marker was there and parsed. An empty `Vec` means the app declared
    /// no contracts, which is a real answer and not the same as `Absent`.
    Present(Vec<PluginContract>),
    /// The marker was there and its payload could not be read: a truncated
    /// pipe, or a `PluginContract` shape this CLI is too old to understand.
    Malformed(String),
}

/// Parse the plugin-contract dump ([`PLUGIN_CONTRACT_MARKER`]) out of a child
/// binary's stderr.
///
/// The **last** marker line wins, matching the security-config dump: a child
/// that re-execs would otherwise be read from its first, stale dump. The last
/// line is selected *before* parsing, so a malformed final dump reports
/// [`ContractDump::Malformed`] rather than silently falling back to an earlier,
/// stale one.
fn parse_plugin_contracts(stderr: &str) -> ContractDump {
    let Some(payload) = stderr
        .lines()
        .filter_map(|line| line.strip_prefix(PLUGIN_CONTRACT_MARKER))
        .next_back()
    else {
        return ContractDump::Absent;
    };

    match serde_json::from_str::<Vec<PluginContract>>(payload.trim()) {
        Ok(contracts) => ContractDump::Present(contracts),
        Err(e) => ContractDump::Malformed(e.to_string()),
    }
}

/// The contract the checked plugin declared, matched on either identity it has.
///
/// [`PluginContract::plugin`] is the plugin's *crate* name and
/// [`PluginContract::registered_as`] is its
/// [`Plugin::name`](autumn_web::plugin::Plugin::name) — which route attribution
/// keys on, and which defaults to `std::any::type_name`. A plugin that declares
/// `env!("CARGO_PKG_NAME")` without overriding `name()` therefore has two
/// identities, and `--plugin-name` takes one string. Matching either means no
/// plugin is unfindable for having picked the other.
fn find_declared<'a>(
    contracts: &'a [PluginContract],
    plugin_name: &str,
) -> Option<&'a PluginContract> {
    contracts
        .iter()
        .find(|c| c.plugin == plugin_name || c.registered_as.as_deref() == Some(plugin_name))
}

/// Check that the plugin declares a usable `autumn-web` compatibility range.
///
/// A declaration is the whole point of the contract, so a plugin that ships
/// none fails here — this is the author-facing gate, and "you have not declared
/// what you support" is exactly what it is for. A declaration that cannot be
/// parsed fails too: it disables the framework's runtime guard while looking
/// like it is armed.
fn check_plugin_contract(
    plugin_name: &str,
    dump: &ContractDump,
    declared: Option<&PluginContract>,
) -> CheckResult {
    let name = "plugin-contract".to_owned();

    let all = match dump {
        ContractDump::Absent => {
            return CheckResult {
                name,
                status: CheckStatus::Skip,
                message: format!(
                    "the built binary emitted no plugin-contract dump; rebuild against an \
                     autumn-web newer than {} to have the contract checked",
                    autumn_web::plugin_contract::AUTUMN_WEB_VERSION
                ),
                diagnostics: vec![],
            };
        }
        ContractDump::Malformed(reason) => {
            // NOT a skip. The binary answered and the answer was unreadable, so
            // nothing about this plugin's contract has been verified — saying
            // "skipped, too old" would be false and would read as green.
            return CheckResult {
                name,
                status: CheckStatus::Fail,
                message: "the built binary emitted a plugin-contract dump that could not be read"
                    .to_owned(),
                diagnostics: vec![
                    reason.clone(),
                    "the contract was NOT checked; this is not the same as a binary that \
                     predates the dump"
                        .to_owned(),
                ],
            };
        }
        ContractDump::Present(all) => all,
    };

    let Some(contract) = declared else {
        let found: Vec<String> = all
            .iter()
            .map(|c| match &c.registered_as {
                Some(registered) if registered != &c.plugin => {
                    format!("{} (registered as `{registered}`)", c.plugin)
                }
                _ => c.plugin.clone(),
            })
            .collect();
        let mut diagnostics = vec![format!(
            "implement `Plugin::contract` on {plugin_name} and return \
             `PluginContract::new(env!(\"CARGO_PKG_NAME\")).autumn_web(\"<range>\")`"
        )];
        if found.is_empty() {
            diagnostics.push("no plugin in this app declares a contract".to_owned());
        } else {
            diagnostics.push(format!(
                "contracts found in this app: {} — `--plugin-name` must match a contract's crate \
                 name or the name the plugin registered under",
                found.join(", ")
            ));
        }
        return CheckResult {
            name,
            status: CheckStatus::Fail,
            message: format!("{plugin_name} declares no autumn-web compatibility contract"),
            diagnostics,
        };
    };

    let Some(requirement) = contract.autumn_web.as_deref() else {
        return CheckResult {
            name,
            status: CheckStatus::Fail,
            message: format!("{plugin_name} returns a contract but declares no autumn-web range"),
            diagnostics: vec![
                "add `.autumn_web(\"<range>\")` — e.g. `\"0.7\"` for a single minor series"
                    .to_owned(),
            ],
        };
    };

    // The framework already enforced the real pairing at registration (the
    // binary would not have got this far otherwise), so what is left to catch
    // here is a requirement no parser can evaluate — which silently disables
    // that enforcement.
    match evaluate(contract, autumn_web::plugin_contract::AUTUMN_WEB_VERSION) {
        ContractVerdict::Unparseable { reason, .. } => CheckResult {
            name,
            status: CheckStatus::Fail,
            message: format!("{plugin_name} declares an autumn-web range that cannot be evaluated"),
            diagnostics: vec![
                reason,
                "an unevaluable range disables the framework's startup compatibility check"
                    .to_owned(),
            ],
        },
        _ => CheckResult {
            name,
            status: CheckStatus::Pass,
            message: format!("{} supports autumn-web {requirement}", contract.label()),
            diagnostics: vec![],
        },
    }
}

/// Report the experimental plugin surface this plugin declares (issue #1601).
///
/// Reports rather than gates by default — see
/// [`autumn_web::plugin_conformance::check_experimental_surface`], whose rules
/// this mirrors for the CLI's own report type. `deny_experimental` (the
/// `--deny-experimental` flag) turns a clean report into a hard failure for
/// authors who want to forbid it in their own CI.
fn check_experimental_surface(
    dump: &ContractDump,
    declared: Option<&PluginContract>,
    deny: bool,
) -> CheckResult {
    let name = "experimental-surface".to_owned();

    // `--deny-experimental` is a "forbid it" flag, so it fails closed: when the
    // contract could not be read there is no evidence the plugin is clean, and
    // a silent Skip would turn the gate into a no-op exactly when it matters.
    let unevaluated = |message: String| CheckResult {
        name: name.clone(),
        status: if deny {
            CheckStatus::Fail
        } else {
            CheckStatus::Skip
        },
        message: if deny {
            format!("{message} — cannot honour --deny-experimental without it")
        } else {
            message
        },
        diagnostics: vec![],
    };

    match dump {
        ContractDump::Absent => {
            return unevaluated("the built binary emitted no plugin-contract dump".to_owned());
        }
        ContractDump::Malformed(_) => {
            return unevaluated(
                "the built binary's plugin-contract dump could not be read; see the \
                 plugin-contract check"
                    .to_owned(),
            );
        }
        ContractDump::Present(_) => {}
    }

    let Some(contract) = declared else {
        return unevaluated(
            "no contract declared for this plugin; see the plugin-contract check".to_owned(),
        );
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
    for entry in &contract.experimental_surfaces {
        match surface(entry) {
            None => {
                invalid += 1;
                diagnostics.push(format!(
                    "{entry}: not a known plugin surface — check the spelling against \
                     `autumn_web::plugin_contract::PLUGIN_SURFACES`"
                ));
            }
            Some(s) if s.tier == SurfaceTier::Stable => {
                invalid += 1;
                diagnostics.push(format!(
                    "{entry}: declared experimental, but it is a STABLE surface — drop the \
                     declaration; it overstates this plugin's exposure"
                ));
            }
            Some(s) => diagnostics.push(format!("{}: {}", s.name, s.note)),
        }
    }

    let count = contract.experimental_surfaces.len();
    if invalid > 0 {
        return CheckResult {
            name,
            status: CheckStatus::Fail,
            message: format!(
                "{invalid} of {count} declared experimental surface(s) could not be resolved \
                 against the registry"
            ),
            diagnostics,
        };
    }

    CheckResult {
        name,
        status: if deny {
            CheckStatus::Fail
        } else {
            CheckStatus::Pass
        },
        message: if deny {
            format!("{count} experimental surface(s) declared, and --deny-experimental is set")
        } else {
            format!("depends on {count} experimental surface(s); these may change in any release")
        },
        diagnostics,
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
            contracts: &ContractDump::Absent,
            deny_experimental: false,
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
            contracts: &ContractDump::Absent,
            deny_experimental: false,
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
            contracts: &ContractDump::Absent,
            deny_experimental: false,
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
            contracts: &ContractDump::Absent,
            deny_experimental: false,
        };
        let report = build_report(&opts, &[]);
        assert_eq!(report.plugin_name, "autumn-admin-plugin");
    }
}

#[cfg(test)]
mod contract_tests {
    //! `autumn plugin-check`'s half of the plugin API stability contract
    //! (issue #1601): parsing the contract dump off the child's stderr, and
    //! the two checks built on it.

    use super::*;
    use autumn_web::plugin_contract::{PLUGIN_CONTRACT_MARKER, PluginContract};

    fn opts(contracts: &ContractDump) -> PluginCheckOptions<'_> {
        PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "autumn-plugin-demo",
            expected_prefix: None,
            sensitive_routes: &[],
            format: ReportFormat::Text,
            contracts,
            deny_experimental: false,
        }
    }

    fn present(contracts: Vec<PluginContract>) -> ContractDump {
        ContractDump::Present(contracts)
    }

    fn route() -> RouteInfo {
        RouteInfo {
            method: "GET".to_owned(),
            path: "/demo".to_owned(),
            handler: "demo::index".to_owned(),
            source: "plugin:autumn-plugin-demo".to_owned(),
            middleware: vec![],
            api_version: None,
            status: None,
            sunset_opt_out: None,
            resource_shape: String::new(),
            pools: Vec::new(),
        }
    }

    fn find<'a>(report: &'a ConformanceReport, name: &str) -> &'a CheckResult {
        report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no `{name}` check in report"))
    }

    // ── parsing the dump ───────────────────────────────────────────────────

    #[test]
    fn contracts_are_parsed_from_the_marker_line() {
        let stderr = format!(
            "\u{1F342} autumn plugin-check\nsome warning\n{PLUGIN_CONTRACT_MARKER}[{{\"plugin\":\"autumn-plugin-demo\",\"autumn_web\":\"0.7\"}}]\ntrailing\n"
        );
        let ContractDump::Present(parsed) = parse_plugin_contracts(&stderr) else {
            panic!("marker present");
        };
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].plugin, "autumn-plugin-demo");
        assert_eq!(parsed[0].autumn_web.as_deref(), Some("0.7"));
    }

    #[test]
    fn an_absent_marker_parses_as_absent() {
        assert_eq!(
            parse_plugin_contracts("no marker here\n"),
            ContractDump::Absent
        );
    }

    #[test]
    fn an_empty_contract_array_is_present_and_empty_not_absent() {
        let stderr = format!("{PLUGIN_CONTRACT_MARKER}[]\n");
        let ContractDump::Present(parsed) = parse_plugin_contracts(&stderr) else {
            panic!("marker present");
        };
        assert!(parsed.is_empty(), "the app declared no contracts");
    }

    #[test]
    fn a_malformed_marker_payload_is_distinguished_from_an_absent_one() {
        let stderr = format!("{PLUGIN_CONTRACT_MARKER}{{not json\n");
        assert!(
            matches!(parse_plugin_contracts(&stderr), ContractDump::Malformed(_)),
            "a marker the CLI cannot read is NOT the same as no marker"
        );
    }

    #[test]
    fn the_last_marker_line_wins() {
        let stderr = format!(
            "{PLUGIN_CONTRACT_MARKER}[]\n{PLUGIN_CONTRACT_MARKER}[{{\"plugin\":\"second\"}}]\n"
        );
        let ContractDump::Present(parsed) = parse_plugin_contracts(&stderr) else {
            panic!("marker present");
        };
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].plugin, "second");
    }

    // ── the `plugin-contract` check ────────────────────────────────────────

    #[test]
    fn a_binary_that_emits_no_marker_skips_the_contract_check() {
        let report = build_report(&opts(&ContractDump::Absent), &[route()]);
        let result = find(&report, "plugin-contract");
        assert_eq!(result.status, CheckStatus::Skip);
    }

    #[test]
    fn a_plugin_with_no_contract_fails_the_contract_check() {
        let others = vec![PluginContract::new("some-other-plugin").autumn_web("0.7")];
        let report = build_report(&opts(&present(others)), &[route()]);
        let result = find(&report, "plugin-contract");
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "the checked plugin declares nothing: {}",
            result.message
        );
        assert!(
            result.message.contains("autumn-plugin-demo"),
            "{}",
            result.message
        );
    }

    #[test]
    fn a_declared_range_passes_and_is_reported() {
        let contracts = vec![
            PluginContract::new("autumn-plugin-demo")
                .plugin_version("1.2.3")
                .autumn_web("0.7"),
        ];
        let report = build_report(&opts(&present(contracts)), &[route()]);
        let result = find(&report, "plugin-contract");
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(result.message.contains("0.7"), "{}", result.message);
    }

    #[test]
    fn an_unparseable_range_fails_the_contract_check() {
        let contracts = vec![PluginContract::new("autumn-plugin-demo").autumn_web("not a req")];
        let report = build_report(&opts(&present(contracts)), &[route()]);
        let result = find(&report, "plugin-contract");
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "an unevaluable requirement disables the runtime guard silently"
        );
        assert!(!report.passed());
    }

    // ── the `experimental-surface` check ───────────────────────────────────

    #[test]
    fn a_stable_only_plugin_passes_the_experimental_check() {
        let contracts = vec![PluginContract::new("autumn-plugin-demo").autumn_web("0.7")];
        let report = build_report(&opts(&present(contracts)), &[route()]);
        let result = find(&report, "experimental-surface");
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn experimental_surface_is_reported_by_name_without_failing() {
        let contracts = vec![
            PluginContract::new("autumn-plugin-demo")
                .autumn_web("0.7")
                .uses_experimental("AppBuilder::with_edge_kv"),
        ];
        let report = build_report(&opts(&present(contracts)), &[route()]);
        let result = find(&report, "experimental-surface");
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.contains("AppBuilder::with_edge_kv")),
            "{:?}",
            result.diagnostics
        );
        assert!(report.passed());
    }

    #[test]
    fn deny_experimental_turns_the_report_into_a_failure() {
        let contracts = vec![
            PluginContract::new("autumn-plugin-demo")
                .autumn_web("0.7")
                .uses_experimental("AppBuilder::with_edge_kv"),
        ];
        let dump = present(contracts);
        let mut o = opts(&dump);
        o.deny_experimental = true;
        let report = build_report(&o, &[route()]);
        let result = find(&report, "experimental-surface");
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(!report.passed());
    }

    #[test]
    fn deny_experimental_does_not_fail_a_stable_only_plugin() {
        let contracts = vec![PluginContract::new("autumn-plugin-demo").autumn_web("0.7")];
        let dump = present(contracts);
        let mut o = opts(&dump);
        o.deny_experimental = true;
        let report = build_report(&o, &[route()]);
        assert_eq!(
            find(&report, "experimental-surface").status,
            CheckStatus::Pass
        );
        assert!(report.passed());
    }

    #[test]
    fn an_unknown_experimental_surface_name_fails() {
        let contracts = vec![
            PluginContract::new("autumn-plugin-demo")
                .autumn_web("0.7")
                .uses_experimental("AppBuilder::not_a_surface"),
        ];
        let report = build_report(&opts(&present(contracts)), &[route()]);
        assert_eq!(
            find(&report, "experimental-surface").status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn the_experimental_check_is_skipped_when_the_binary_emits_no_marker() {
        let report = build_report(&opts(&ContractDump::Absent), &[route()]);
        assert_eq!(
            find(&report, "experimental-surface").status,
            CheckStatus::Skip
        );
    }

    /// A malformed FINAL dump must not silently fall back to an earlier, stale
    /// one — which is exactly what parsing before selecting would do.
    #[test]
    fn a_malformed_last_dump_does_not_fall_back_to_an_earlier_one() {
        let stderr = format!(
            "{PLUGIN_CONTRACT_MARKER}[{{\"plugin\":\"stale\"}}]\n{PLUGIN_CONTRACT_MARKER}[{{\"plugin\":\"trunc\"\n"
        );
        assert!(
            matches!(parse_plugin_contracts(&stderr), ContractDump::Malformed(_)),
            "the stale first dump must not win"
        );
    }

    #[test]
    fn a_malformed_dump_fails_the_contract_check_rather_than_skipping_it() {
        let dump = ContractDump::Malformed("expected value at line 1".to_owned());
        let report = build_report(&opts(&dump), &[route()]);
        let result = find(&report, "plugin-contract");
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "an unreadable answer is not the same as no answer"
        );
        assert!(!report.passed());
    }

    #[test]
    fn deny_experimental_fails_closed_when_the_contract_cannot_be_read() {
        for dump in [
            ContractDump::Absent,
            ContractDump::Malformed("bad".to_owned()),
            ContractDump::Present(vec![]),
        ] {
            let mut o = opts(&dump);
            o.deny_experimental = true;
            let report = build_report(&o, &[route()]);
            assert_eq!(
                find(&report, "experimental-surface").status,
                CheckStatus::Fail,
                "--deny-experimental must not silently no-op for {dump:?}"
            );
        }
    }

    /// A plugin that declares `env!("CARGO_PKG_NAME")` but does not override
    /// `Plugin::name()` has two identities. `--plugin-name` takes one string,
    /// so both have to resolve or the plugin is unfindable.
    #[test]
    fn a_contract_is_found_under_the_name_the_plugin_registered_under() {
        let mut contract = PluginContract::new("autumn-plugin-demo").autumn_web("0.7");
        contract.registered_as = Some("demo_crate::DemoPlugin".to_owned());
        let dump = present(vec![contract]);

        for name in ["autumn-plugin-demo", "demo_crate::DemoPlugin"] {
            let mut o = opts(&dump);
            o.plugin_name = name;
            let report = build_report(&o, &[route()]);
            assert_eq!(
                find(&report, "plugin-contract").status,
                CheckStatus::Pass,
                "--plugin-name {name} should resolve"
            );
        }
    }

    #[test]
    fn the_not_found_diagnostic_names_both_identities() {
        let mut contract = PluginContract::new("autumn-plugin-demo").autumn_web("0.7");
        contract.registered_as = Some("demo_crate::DemoPlugin".to_owned());
        let dump = present(vec![contract]);
        let mut o = opts(&dump);
        o.plugin_name = "something-else";
        let report = build_report(&o, &[route()]);
        let joined = find(&report, "plugin-contract").diagnostics.join("\n");
        assert!(joined.contains("autumn-plugin-demo"), "{joined}");
        assert!(joined.contains("demo_crate::DemoPlugin"), "{joined}");
    }

    /// The skip message must not name a hard-coded future release: the version
    /// this CLI links is the only one it can honestly speak about.
    #[test]
    fn the_older_binary_skip_message_names_this_builds_version() {
        let report = build_report(&opts(&ContractDump::Absent), &[route()]);
        let message = &find(&report, "plugin-contract").message;
        assert!(
            message.contains(autumn_web::plugin_contract::AUTUMN_WEB_VERSION),
            "{message}"
        );
    }

    #[test]
    fn the_text_report_renders_the_new_checks() {
        let contracts = vec![
            PluginContract::new("autumn-plugin-demo")
                .autumn_web("0.7")
                .uses_experimental("AppBuilder::with_edge_kv"),
        ];
        let report = build_report(&opts(&present(contracts)), &[route()]);
        let text = report.to_text_report();
        assert!(text.contains("plugin-contract"), "{text}");
        assert!(text.contains("experimental-surface"), "{text}");
    }
}
