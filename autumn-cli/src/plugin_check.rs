//! `autumn plugin-check` — run conformance checks against a plugin's routes.
//!
//! Compiles the target binary (debug profile), runs it with
//! `AUTUMN_DUMP_ROUTES=1` to collect the route manifest, then applies the
//! conformance checks and outputs a pass/fail report.
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
//! | `duplicate-registration` | No plugin route appears more than once |
//! | `plugin-contract` | The plugin declares a parseable `autumn-web` range |
//! | `experimental-surface` | The plugin's declared use of experimental API |
//!
//! The route rules and the report types come from
//! [`autumn_web::plugin_conformance`] (issue #2828). This module collects the
//! routes and adds the checks that need the built binary.
//!
//! The last two checks read the contract the built binary dumps after
//! [`PLUGIN_CONTRACT_MARKER`](autumn_web::plugin_contract::PLUGIN_CONTRACT_MARKER)
//! (issue #1601). A binary that emits no marker — an app built before the
//! contract existed — skips both rather than failing.

use std::process::Command;

use serde::{Deserialize, Serialize};

use autumn_web::plugin_conformance::{self as rules, ConformanceConfig, ConformanceReport};
pub use autumn_web::plugin_conformance::{CheckResult, CheckStatus, SensitiveRoute};
use autumn_web::plugin_contract::{
    ContractVerdict, PLUGIN_CONTRACT_MARKER, PluginContract, evaluate,
};
use autumn_web::route_listing::RouteSource;

use crate::routes::{self, RouteInfo, compile_binary, find_binary};

// ── Report types ───────────────────────────────────────────────────────────

/// The `autumn plugin-check` report: the library report and the data the
/// plugin index reads. Its JSON is the library report's JSON with more keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCheckReport {
    /// The plugin name and the check results.
    #[serde(flatten)]
    pub report: ConformanceReport,
    /// The contract the plugin declared, when the binary dumped one. The
    /// plugin index records its range and tier from this (issue #1625).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<PluginContract>,
    /// The `autumn-web` version the checked app built against, read from its
    /// `Cargo.lock`. The plugin index records a report only for this release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autumn_web: Option<String>,
    /// The `--prefix` the `route-prefix` check tested. The plugin index
    /// records a pass for a prefixed listing only when this is its prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Root-level paths `--intentional-root` exempted from `route-prefix`
    /// (issue #2828). A pass that leans on them is not evidence for a plugin
    /// index listing, which cannot replay the exemption, so curation refuses
    /// such a report.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intentional_root: Vec<String>,
}

impl PluginCheckReport {
    /// A report with the given checks and no index data.
    pub fn new(plugin_name: impl Into<String>, checks: Vec<CheckResult>) -> Self {
        Self {
            report: ConformanceReport {
                plugin_name: plugin_name.into(),
                checks,
            },
            contract: None,
            autumn_web: None,
            prefix: None,
            intentional_root: Vec::new(),
        }
    }

    /// Returns `true` when no check has `CheckStatus::Fail`.
    pub fn passed(&self) -> bool {
        self.report.passed()
    }

    /// The report as human-readable text.
    pub fn to_text_report(&self) -> String {
        self.report.to_text_report()
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
    /// Paths intentionally served at the root level, exempt from the
    /// route-prefix check (exact path match). The CLI-side spelling of the
    /// library API's `ConformanceConfig::intentional_root_routes`
    /// (issue #2828): a route the library harness can declare via
    /// `.intentional_root_route(...)` must be declarable to
    /// `autumn plugin-check` as well, or the CLI reports a false
    /// `route-prefix` FAIL for the same route the crate's own test passes.
    pub intentional_root_routes: &'a [String],
    /// Declared sensitive routes with their auth mechanisms.
    pub sensitive_routes: &'a [SensitiveRoute],
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
    /// `--no-routes`: the plugin asserts it mounts no routes (a cache, a
    /// search index). Only then may `route-attribution` skip when no routes
    /// are found. The contract dump must also give `registered_as`.
    pub no_routes: bool,
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
    let mut command = Command::new(&binary);
    command.env(routes::DUMP_ENV, "1");
    command.env("AUTUMN_DUMP_PLUGIN_CONTRACT", "1");
    routes::clear_other_dump_modes(&mut command, routes::DUMP_ENV);
    let output = command
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

    let mut report = build_report(
        &PluginCheckOptions {
            package: opts.package,
            bin: opts.bin,
            plugin_name: opts.plugin_name,
            expected_prefix: opts.expected_prefix,
            intentional_root_routes: opts.intentional_root_routes,
            sensitive_routes: opts.sensitive_routes,
            format: opts.format.clone(),
            contracts: &contracts,
            deny_experimental: opts.deny_experimental,
            no_routes: opts.no_routes,
        },
        &routes,
    );
    // Routes behind a raw `.merge()`/`.nest()` were never listed, so no
    // check above saw them.
    fail_on_omitted_routers(
        &mut report,
        crate::routes_audit::parse_omitted_count(&stderr),
    );
    // Without `-p`, the package is the member owning the binary that ran,
    // named by `--bin` or picked implicitly by `find_binary`.
    let package = opts.package.map(str::to_owned).or_else(|| {
        opts.bin
            .map(str::to_owned)
            .or_else(|| {
                binary
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_owned)
            })
            .and_then(|bin| package_owning_bin(&bin))
    });
    report.autumn_web = locked_autumn_web(std::path::Path::new("."), package.as_deref());

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
pub fn build_report(opts: &PluginCheckOptions<'_>, routes: &[RouteInfo]) -> PluginCheckReport {
    let mut checks = vec![CheckResult {
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
    }];
    let routes = library_routes(routes);

    let declared = match opts.contracts {
        ContractDump::Present(all) => find_declared(all, opts.plugin_name),
        ContractDump::Absent | ContractDump::Malformed(_) => None,
    };

    let mut config = ConformanceConfig::new(opts.plugin_name);
    config.expected_prefix = opts.expected_prefix.map(str::to_owned);
    config.intentional_root_routes = opts.intentional_root_routes.to_vec();
    config.sensitive_routes = opts.sensitive_routes.to_vec();
    config.contract = declared.cloned();
    config.no_routes = opts.no_routes;
    let mut route_checks = rules::run_route_checks(&config, &routes);

    // The library lets `no_routes` skip with no contract. The CLI skips only
    // when the contract dump gives the registered name. In all other cases,
    // no routes found fails.
    if opts.no_routes
        && declared.and_then(|c| c.registered_as.as_ref()).is_none()
        && let Some(attribution) = route_checks
            .iter_mut()
            .find(|c| c.name == "route-attribution" && c.status == CheckStatus::Skip)
    {
        attribution.status = CheckStatus::Fail;
        attribution.message = format!(
            "--no-routes skips only when the contract dump gives the registered name of \
             {}. The dump gives none",
            opts.plugin_name
        );
    }
    checks.append(&mut route_checks);

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

    let mut report = PluginCheckReport::new(opts.plugin_name, checks);
    report.contract = declared.cloned();
    report.prefix = opts.expected_prefix.map(str::to_owned);
    report.intentional_root = opts.intentional_root_routes.to_vec();
    report
}

/// Converts the dumped routes to the library `RouteInfo`, so that the library
/// rules can run on them. The checks read only the fields that this function
/// sets.
fn library_routes(routes: &[RouteInfo]) -> Vec<autumn_web::route_listing::RouteInfo> {
    routes
        .iter()
        .map(|r| autumn_web::route_listing::RouteInfo {
            method: r.method.clone(),
            path: r.path.clone(),
            handler: r.handler.clone(),
            source: RouteSource::from_wire(&r.source),
            middleware: r.middleware.clone(),
            ..Default::default()
        })
        .collect()
}

/// The `autumn-web` version the workspace's `Cargo.lock` resolved for the app
/// in `dir`. Cargo uses the workspace root's lockfile, not a member's. With
/// `package` (`-p`, else the manifest's own package) it follows that
/// package's dependency edge, so another member on another `autumn-web` does
/// not make it ambiguous. `None` when there is no lockfile, or the version
/// cannot be told apart.
pub fn locked_autumn_web(dir: &std::path::Path, package: Option<&str>) -> Option<String> {
    crate::plugin::install::locked_version_for(dir, package, "autumn-web")
}

/// Fail `route-attribution` when the dump omitted raw routers: their routes
/// were never attributed, prefix-checked or reviewed for sensitive paths.
fn fail_on_omitted_routers(report: &mut PluginCheckReport, omitted: usize) {
    if omitted == 0 {
        return;
    }
    if let Some(check) = report
        .report
        .checks
        .iter_mut()
        .find(|check| check.name == "route-attribution")
    {
        check.status = CheckStatus::Fail;
        check.message = format!(
            "{omitted} raw router(s) added via .merge()/.nest() are not enumerable, so their \
             routes were not checked. Declare them with AppBuilder::declare_plugin_routes"
        );
    }
}

/// The workspace package with a `bin` target named `bin`, from `cargo
/// metadata`. `None` when none or several do.
fn package_owning_bin(bin: &str) -> Option<String> {
    let output = std::process::Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .ok()?;
    let metadata = serde_json::from_slice(&output.stdout).ok()?;
    package_of_bin(&metadata, bin)
}

fn package_of_bin(metadata: &serde_json::Value, bin: &str) -> Option<String> {
    let mut owners = metadata["packages"]
        .as_array()?
        .iter()
        .filter(|package| {
            package["targets"].as_array().is_some_and(|targets| {
                targets.iter().any(|target| {
                    target["name"] == bin
                        && target["kind"]
                            .as_array()
                            .is_some_and(|kinds| kinds.iter().any(|k| k == "bin"))
                })
            })
        })
        .filter_map(|package| package["name"].as_str());
    let owner = owners.next()?;
    owners.next().is_none().then(|| owner.to_owned())
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
/// Reports rather than gates by default. The rule is
/// [`autumn_web::plugin_conformance::check_experimental_surface`]. This
/// function adds the contract-dump outcomes. `deny_experimental` (the
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

    // The library rule, and then `--deny-experimental` on top of it.
    let mut result = rules::check_experimental_surface(Some(contract));
    let count = contract.experimental_surfaces.len();
    if deny && count > 0 && result.status == CheckStatus::Pass {
        result.status = CheckStatus::Fail;
        result.message =
            format!("{count} experimental surface(s) declared, and --deny-experimental is set");
    }
    result
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod shared_rules_tests {
    use super::*;
    use autumn_web::plugin_conformance::{self as lib, ConformanceConfig};
    use autumn_web::plugin_contract::PluginContract;

    /// The CLI must reuse the library's rules and report types (#2828).
    /// A local copy would drift from `autumn_web::plugin_conformance`.
    #[test]
    fn rules_and_report_types_are_not_redefined_here() {
        let src = include_str!("plugin_check.rs");
        let body = &src[..src.find("mod shared_rules_tests").unwrap_or(src.len())];
        for name in [
            "fn is_sensitive_path",
            "fn is_plugin_asset_path",
            "fn normalize_path_for_collision",
            "fn is_plugin_asset_route",
            "const SENSITIVE_KEYWORDS",
            "fn check_route_attribution",
            "fn check_route_prefix",
            "fn check_sensitive_surfaces",
            "fn check_duplicate_registration",
            "fn check_collisions",
            "fn check_no_routes",
            "fn check_split_attribution",
            "fn route_key",
            "fn carries",
            "fn route_name",
            "fn plugin_names",
            "fn check_attribution",
            "fn check_prefix_with_no_routes",
            "enum CheckStatus",
            "struct CheckResult",
            "struct ConformanceReport",
            "struct SensitiveRoute",
            "Plugin conformance: ",
        ] {
            assert!(!body.contains(name), "`{name}` must live in the library");
        }
    }

    fn route(method: &str, path: &str, source: &str) -> RouteInfo {
        RouteInfo {
            method: method.to_owned(),
            path: path.to_owned(),
            handler: format!("h{path}"),
            source: source.to_owned(),
            middleware: vec![],
            api_version: None,
            status: None,
            sunset_opt_out: None,
            resource_shape: String::new(),
            pools: Vec::new(),
            group: None,
        }
    }

    fn asset(path: &str, source: &str) -> RouteInfo {
        let mut asset = route("GET", path, source);
        asset.middleware = vec![autumn_web::assets::PLUGIN_ASSETS_ROUTE_MARKER.to_owned()];
        asset
    }

    fn options<'a>(
        plugin_name: &'a str,
        sensitive: &'a [lib::SensitiveRoute],
        root: &'a [String],
        dump: &'a ContractDump,
    ) -> PluginCheckOptions<'a> {
        PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name,
            expected_prefix: Some("/shop"),
            intentional_root_routes: root,
            sensitive_routes: sensitive,
            format: ReportFormat::Text,
            contracts: dump,
            deny_experimental: false,
            no_routes: false,
        }
    }

    fn fixtures(name: &str) -> Vec<Vec<RouteInfo>> {
        let src = format!("plugin:{name}");
        let src = src.as_str();
        vec![
            vec![route("GET", "/shop", src)],
            vec![route("GET", "/shop", src), route("POST", "/shop/{id}", src)],
            vec![route("GET", "/other", src)],
            vec![route("GET", "/hook", src)],
            vec![
                route("POST", "/shop/{id}", "user"),
                route("POST", "/shop/{item}", src),
                route("GET", "/a/{x}", "framework"),
                route("GET", "/a/{*rest}", "plugin:other"),
            ],
            vec![
                route("GET", "/shop/admin", src),
                route("GET", "/shop/debug", src),
            ],
            vec![route("GET", "/shop/debug", src)],
            vec![route("GET", "/shop", src), route("GET", "/shop", src)],
            vec![
                route("GET", "/shop", src),
                asset("/static/_plugins/shop/admin.js", src),
            ],
            vec![route("GET", "/static/_plugins/x/admin", src)],
            vec![route("GET", "/x", "user")],
        ]
    }

    /// Compare every check the library runs, except `experimental-surface`
    /// (the CLI reads it from the contract dump).
    fn assert_same_route_checks(
        cli: &PluginCheckReport,
        library: &lib::ConformanceReport,
        routes: &[RouteInfo],
    ) {
        for check in library
            .checks
            .iter()
            .filter(|c| c.name != "experimental-surface")
        {
            let found = cli
                .report
                .checks
                .iter()
                .find(|c| c.name == check.name)
                .unwrap_or_else(|| panic!("the CLI does not run `{}`", check.name));
            assert_eq!(
                (&found.status, &found.message, &found.diagnostics),
                (&check.status, &check.message, &check.diagnostics),
                "`{}` differs for {routes:?}",
                check.name
            );
        }
    }

    /// For the same routes, `autumn plugin-check` and `run_conformance` give
    /// the same status, message and diagnostics for each route check (#2828).
    #[test]
    fn route_checks_match_the_library() {
        let root = vec!["/hook".to_owned()];
        let sensitive = vec![lib::SensitiveRoute {
            path_pattern: "/shop/debug".to_owned(),
            auth_mechanism: "Role: ops".to_owned(),
        }];
        let base = ConformanceConfig::new("shop")
            .prefix("/shop")
            .intentional_root_route("/hook")
            .sensitive_route("/shop/debug", "Role: ops");

        // No contract dump.
        for routes in &fixtures("shop") {
            let cli = build_report(
                &options("shop", &sensitive, &root, &ContractDump::Absent),
                routes,
            );
            let library = lib::run_conformance(&base, &by_serde(routes));
            assert_same_route_checks(&cli, &library, routes);
        }

        // A contract whose registered name carries the routes.
        let mut contract = PluginContract::new("shop").autumn_web("0.7");
        contract.registered_as = Some("shop::Shop".to_owned());
        let dump = ContractDump::Present(vec![contract.clone()]);
        let config = base.contract(contract);
        let mut with_contract = fixtures("shop::Shop");
        with_contract.push(vec![
            route("GET", "/shop", "plugin:shop"),
            route("GET", "/shop/x", "plugin:shop::Shop"),
        ]);
        for routes in &with_contract {
            let cli = build_report(&options("shop", &sensitive, &root, &dump), routes);
            let library = lib::run_conformance(&config, &by_serde(routes));
            assert_same_route_checks(&cli, &library, routes);
        }

        // `--no-routes` with a registered name: the CLI and the library agree.
        let mut config = config.no_routes();
        config.expected_prefix = None;
        let mut opts = options("shop", &sensitive, &root, &dump);
        opts.expected_prefix = None;
        opts.no_routes = true;
        for routes in &with_contract {
            let cli = build_report(&opts, routes);
            let library = lib::run_conformance(&config, &by_serde(routes));
            assert_same_route_checks(&cli, &library, routes);
        }
    }

    /// The library routes, made through serde rather than `library_routes`, so
    /// that the parity test also checks the conversion.
    fn by_serde(routes: &[RouteInfo]) -> Vec<autumn_web::route_listing::RouteInfo> {
        let json = serde_json::to_value(routes).expect("json");
        serde_json::from_value(json).expect("library routes")
    }

    /// The conversion keeps what the checks read: the source and the asset
    /// marker.
    #[test]
    fn library_routes_keeps_source_and_middleware() {
        let converted = library_routes(&[asset("/static/_plugins/shop/a.js", "plugin:shop")]);
        assert_eq!(converted[0].source, RouteSource::Plugin("shop".to_owned()));
        assert_eq!(
            converted[0].middleware,
            [autumn_web::assets::PLUGIN_ASSETS_ROUTE_MARKER]
        );
        assert_eq!(converted[0].path, "/static/_plugins/shop/a.js");
        assert_eq!(converted[0].method, "GET");
    }

    /// `--no-routes` counts a route under any name of the plugin, as the
    /// library does. Here `--plugin-name` is the registered name, and the
    /// route carries the crate name.
    #[test]
    fn no_routes_counts_every_name_of_the_plugin() {
        let mut contract = PluginContract::new("autumn-plugin-demo").autumn_web("0.7");
        contract.registered_as = Some("demo::DemoPlugin".to_owned());
        let dump = ContractDump::Present(vec![contract]);
        let mut opts = options("demo::DemoPlugin", &[], &[], &dump);
        opts.expected_prefix = None;
        opts.no_routes = true;
        let attribution = |routes: &[RouteInfo]| {
            build_report(&opts, routes)
                .report
                .checks
                .into_iter()
                .find(|c| c.name == "route-attribution")
                .expect("route-attribution")
        };
        let result = attribution(&[route("GET", "/x", "plugin:autumn-plugin-demo")]);
        assert_eq!(result.status, CheckStatus::Fail, "{}", result.message);
        assert!(
            result.message.starts_with("no_routes was declared"),
            "{}",
            result.message
        );
        assert_eq!(attribution(&[]).status, CheckStatus::Skip);
    }

    /// `--no-routes` without a registered name never passes: routes under
    /// the plugin name contradict it, and no routes cannot skip.
    #[test]
    fn no_routes_without_a_registered_name_never_passes() {
        let mut opts = options("shop", &[], &[], &ContractDump::Absent);
        opts.expected_prefix = None;
        opts.no_routes = true;
        for routes in [vec![route("GET", "/shop", "plugin:shop")], vec![]] {
            let report = build_report(&opts, &routes);
            let attribution = &report.report.checks[1];
            assert_eq!(attribution.name, "route-attribution");
            assert_eq!(
                attribution.status,
                CheckStatus::Fail,
                "{routes:?}: {}",
                attribution.message
            );
            if routes.is_empty() {
                assert!(
                    attribution.message.starts_with("--no-routes skips only"),
                    "{}",
                    attribution.message
                );
            }
        }
    }

    /// The CLI JSON report reads as the library report: one wire shape.
    #[test]
    fn the_json_report_reads_as_the_library_report() {
        let dump = ContractDump::Absent;
        let root = vec!["/hook".to_owned()];
        let report = build_report(
            &options("shop", &[], &root, &dump),
            &[route("GET", "/shop", "plugin:shop")],
        );
        let json = serde_json::to_string(&report).expect("json");
        let library: lib::ConformanceReport = serde_json::from_str(&json).expect("library");
        assert_eq!(library.plugin_name, "shop");
        assert_eq!(library.checks[0].name, "installability");
        let back: PluginCheckReport = serde_json::from_str(&json).expect("cli");
        assert_eq!(back.prefix.as_deref(), Some("/shop"));
        assert_eq!(back.intentional_root, root);
    }
}

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
            group: None,
        }
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

    // ── build_report ───────────────────────────────────────────────────────

    #[test]
    fn build_report_includes_installability_check() {
        let opts = PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "test",
            expected_prefix: None,
            intentional_root_routes: &[],
            sensitive_routes: &[],
            format: ReportFormat::Text,
            contracts: &ContractDump::Absent,
            deny_experimental: false,
            no_routes: false,
        };
        let routes = vec![make_route("GET", "/posts", "user")];
        let report = build_report(&opts, &routes);
        assert!(
            report
                .report
                .checks
                .iter()
                .any(|c| c.name == "installability")
        );
    }

    #[test]
    fn build_report_skips_prefix_check_when_none() {
        let opts = PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "test",
            expected_prefix: None,
            intentional_root_routes: &[],
            sensitive_routes: &[],
            format: ReportFormat::Text,
            contracts: &ContractDump::Absent,
            deny_experimental: false,
            no_routes: false,
        };
        let report = build_report(&opts, &[]);
        assert!(
            !report
                .report
                .checks
                .iter()
                .any(|c| c.name == "route-prefix")
        );
    }

    #[test]
    fn build_report_includes_prefix_check_when_set() {
        let opts = PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "admin",
            expected_prefix: Some("/admin"),
            intentional_root_routes: &[],
            sensitive_routes: &[],
            format: ReportFormat::Text,
            contracts: &ContractDump::Absent,
            deny_experimental: false,
            no_routes: false,
        };
        let routes = vec![make_route("GET", "/admin", "plugin:admin")];
        let report = build_report(&opts, &routes);
        assert!(
            report
                .report
                .checks
                .iter()
                .any(|c| c.name == "route-prefix")
        );
        // The report names the prefix it tested, for `plugin index record`.
        assert_eq!(report.prefix.as_deref(), Some("/admin"));
    }

    /// A root-level route the operator declared via `--intentional-root` is
    /// exempt from the prefix check (issue #2828): the CLI must not fail
    /// what the library API's `.intentional_root_route(...)` lets pass.
    #[test]
    fn build_report_exempts_declared_intentional_root_routes() {
        let declared = vec!["/webhook".to_owned()];
        let opts = PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "admin",
            expected_prefix: Some("/admin"),
            intentional_root_routes: &declared,
            sensitive_routes: &[],
            format: ReportFormat::Text,
            contracts: &ContractDump::Absent,
            deny_experimental: false,
            no_routes: false,
        };
        let routes = vec![
            make_route("GET", "/admin", "plugin:admin"),
            make_route("POST", "/webhook", "plugin:admin"),
        ];
        let report = build_report(&opts, &routes);
        let check = report
            .report
            .checks
            .iter()
            .find(|c| c.name == "route-prefix")
            .expect("route-prefix check ran");
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.diagnostics.is_empty());
        // The report names what it exempted, so the plugin index can tell a
        // pass that leaned on the exemption from one that did not.
        assert_eq!(report.intentional_root, vec!["/webhook".to_owned()]);
        let json = serde_json::to_value(&report).expect("serialize");
        assert_eq!(json["intentional_root"], serde_json::json!(["/webhook"]));
    }

    /// The exemption is an exact-path match, like the library's: declaring
    /// `/webhook2` does not cover `/webhook`.
    #[test]
    fn build_report_intentional_root_exemption_is_exact() {
        let declared = vec!["/webhook2".to_owned()];
        let opts = PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "admin",
            expected_prefix: Some("/admin"),
            intentional_root_routes: &declared,
            sensitive_routes: &[],
            format: ReportFormat::Text,
            contracts: &ContractDump::Absent,
            deny_experimental: false,
            no_routes: false,
        };
        let routes = vec![make_route("POST", "/webhook", "plugin:admin")];
        let report = build_report(&opts, &routes);
        let check = report
            .report
            .checks
            .iter()
            .find(|c| c.name == "route-prefix")
            .expect("route-prefix check ran");
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.diagnostics.iter().any(|d| d.contains("/webhook")));
        assert!(
            check
                .message
                .contains("not declared as intentional root routes"),
            "fail message names the exemption: {check:?}"
        );
    }

    /// Without any declaration, an off-prefix route still fails — the new
    /// exemption must not silently widen the check.
    #[test]
    fn build_report_off_prefix_route_still_fails_without_declaration() {
        let opts = PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "admin",
            expected_prefix: Some("/admin"),
            intentional_root_routes: &[],
            sensitive_routes: &[],
            format: ReportFormat::Text,
            contracts: &ContractDump::Absent,
            deny_experimental: false,
            no_routes: false,
        };
        let routes = vec![
            make_route("GET", "/admin", "plugin:admin"),
            make_route("POST", "/webhook", "plugin:admin"),
        ];
        let report = build_report(&opts, &routes);
        let check = report
            .report
            .checks
            .iter()
            .find(|c| c.name == "route-prefix")
            .expect("route-prefix check ran");
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.diagnostics.iter().any(|d| d.contains("/webhook")));
    }

    #[test]
    fn build_report_plugin_name_in_report() {
        let opts = PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "autumn-admin-plugin",
            expected_prefix: None,
            intentional_root_routes: &[],
            sensitive_routes: &[],
            format: ReportFormat::Text,
            contracts: &ContractDump::Absent,
            deny_experimental: false,
            no_routes: false,
        };
        let report = build_report(&opts, &[]);
        assert_eq!(report.report.plugin_name, "autumn-admin-plugin");
    }
}

#[cfg(test)]
mod contract_tests {
    //! `autumn plugin-check`'s half of the plugin API stability contract
    //! (issue #1601): parsing the contract dump off the child's stderr, and
    //! the two checks built on it.

    use super::*;
    use autumn_web::plugin_contract::{PLUGIN_CONTRACT_MARKER, PluginContract};

    /// The tested release is read from `Cargo.lock`, up to the workspace
    /// root; two locked `autumn-web`s are ambiguous.
    #[test]
    fn locked_autumn_web_reads_the_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n",
        )
        .unwrap();
        assert_eq!(locked_autumn_web(&app, None), None);
        let pkg = |v: &str| format!("[[package]]\nname = \"autumn-web\"\nversion = \"{v}\"\n\n");
        let lock = format!(
            "version = 4\n\n{}[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\n",
            pkg("0.7.1")
        );
        std::fs::write(tmp.path().join("Cargo.lock"), &lock).unwrap();
        assert_eq!(locked_autumn_web(&app, None).as_deref(), Some("0.7.1"));
        // A stale member lockfile is not the one Cargo builds with.
        std::fs::write(
            app.join("Cargo.lock"),
            format!("version = 4\n\n{}", pkg("0.1.0")),
        )
        .unwrap();
        assert_eq!(locked_autumn_web(&app, None).as_deref(), Some("0.7.1"));
        std::fs::write(
            tmp.path().join("Cargo.lock"),
            format!("version = 4\n\n{}{}", pkg("0.7.1"), pkg("0.6.0")),
        )
        .unwrap();
        assert_eq!(locked_autumn_web(&app, None), None);
        // The selected package's own edge tells two locked versions apart.
        let host = "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n\
                    dependencies = [\"autumn-web 0.7.1\", \"serde\"]\n\n";
        let other = "[[package]]\nname = \"other\"\nversion = \"0.1.0\"\n\
                     dependencies = [\"autumn-web 0.6.0\"]\n\n";
        std::fs::write(
            tmp.path().join("Cargo.lock"),
            format!(
                "version = 4\n\n{host}{other}{}{}",
                pkg("0.7.1"),
                pkg("0.6.0")
            ),
        )
        .unwrap();
        assert_eq!(locked_autumn_web(&app, None).as_deref(), Some("0.7.1"));
        assert_eq!(
            locked_autumn_web(tmp.path(), Some("other")).as_deref(),
            Some("0.6.0")
        );
        // A bare edge names the one locked version.
        std::fs::write(
            tmp.path().join("Cargo.lock"),
            format!(
                "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n\
                 dependencies = [\"autumn-web\"]\n\n{}",
                pkg("0.7.1")
            ),
        )
        .unwrap();
        assert_eq!(locked_autumn_web(&app, None).as_deref(), Some("0.7.1"));
        // Two edges from one package (an alias) cannot be told apart.
        std::fs::write(
            tmp.path().join("Cargo.lock"),
            format!(
                "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n\
                 dependencies = [\"autumn-web 0.7.1\", \"autumn-web 0.6.0\"]\n\n{}{}",
                pkg("0.7.1"),
                pkg("0.6.0")
            ),
        )
        .unwrap();
        assert_eq!(locked_autumn_web(&app, None), None);
    }

    /// An omitted raw router fails the report, even when the listed routes
    /// pass: its routes were never checked.
    #[test]
    fn omitted_routers_fail_route_attribution() {
        let mut report = PluginCheckReport::new(
            "autumn-plugin-x",
            vec![CheckResult {
                name: "route-attribution".to_owned(),
                status: CheckStatus::Pass,
                message: String::new(),
                diagnostics: vec![],
            }],
        );
        fail_on_omitted_routers(&mut report, 0);
        assert!(report.passed());
        fail_on_omitted_routers(&mut report, 2);
        assert!(!report.passed());
        assert!(report.report.checks[0].message.contains("2 raw router(s)"));
    }

    /// `--bin` without `-p` names the member that owns that binary.
    #[test]
    fn package_of_bin_finds_the_one_owner() {
        let metadata = serde_json::json!({"packages": [
            {"name": "host", "targets": [{"name": "host", "kind": ["bin"]}]},
            {"name": "lib", "targets": [{"name": "host", "kind": ["lib"]}]},
            {"name": "other", "targets": [{"name": "tool", "kind": ["bin"]}]},
        ]});
        assert_eq!(package_of_bin(&metadata, "host").as_deref(), Some("host"));
        assert_eq!(package_of_bin(&metadata, "missing"), None);
        let twice = serde_json::json!({"packages": [
            {"name": "a", "targets": [{"name": "x", "kind": ["bin"]}]},
            {"name": "b", "targets": [{"name": "x", "kind": ["bin"]}]},
        ]});
        assert_eq!(package_of_bin(&twice, "x"), None);
    }

    fn opts(contracts: &ContractDump) -> PluginCheckOptions<'_> {
        PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: "autumn-plugin-demo",
            expected_prefix: None,
            intentional_root_routes: &[],
            sensitive_routes: &[],
            format: ReportFormat::Text,
            contracts,
            deny_experimental: false,
            no_routes: false,
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
            group: None,
        }
    }

    fn find<'a>(report: &'a PluginCheckReport, name: &str) -> &'a CheckResult {
        report
            .report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no `{name}` check in report"))
    }

    // ── route-less plugins (issue #1625) ───────────────────────────────────

    fn demo_contract() -> PluginContract {
        let mut contract = PluginContract::new("autumn-plugin-demo").autumn_web("0.7");
        contract.registered_as = Some("autumn-plugin-demo".to_owned());
        contract
    }

    /// A cache or search plugin mounts no routes. Its contract proves it is
    /// registered under this name, so no routes is not a wrong name.
    #[test]
    fn a_registered_plugin_with_no_routes_skips_route_attribution() {
        let dump = present(vec![demo_contract()]);
        let mut o = opts(&dump);
        o.no_routes = true;
        let report = build_report(&o, &[]);
        assert_eq!(find(&report, "route-attribution").status, CheckStatus::Skip);
        assert!(report.passed(), "{}", report.to_text_report());
    }

    /// `--no-routes` together with `--prefix` is a contradiction, not a
    /// combination that passes on nothing: the same rule
    /// `autumn_web::plugin_conformance::run_conformance` enforces for
    /// `ConformanceConfig::no_routes` (issue #2828's drift class — this is
    /// the library's own special case, mirrored here, not a new rule).
    #[test]
    fn no_routes_with_a_prefix_fails_route_prefix_instead_of_skipping() {
        let dump = present(vec![demo_contract()]);
        let mut o = opts(&dump);
        o.no_routes = true;
        o.expected_prefix = Some("/demo");
        let report = build_report(&o, &[]);
        let prefix = find(&report, "route-prefix");
        assert_eq!(prefix.status, CheckStatus::Fail, "{}", prefix.message);
        assert!(prefix.message.contains("no_routes") || prefix.message.contains("--no-routes"));
        assert!(!report.passed());
    }

    /// No routes skips only on an explicit `--no-routes`: an unannotated
    /// `nest` also dumps none, and must not pass unchecked.
    #[test]
    fn no_routes_without_the_assertion_fails_attribution() {
        let dump = present(vec![demo_contract()]);
        let report = build_report(&opts(&dump), &[]);
        assert_eq!(find(&report, "route-attribution").status, CheckStatus::Fail);
        // And the assertion is refused when routes are there.
        let mut o = opts(&dump);
        o.no_routes = true;
        let report = build_report(&o, &[route()]);
        let attribution = find(&report, "route-attribution");
        assert_eq!(attribution.status, CheckStatus::Fail);
        assert!(
            attribution.message.contains("no_routes"),
            "{}",
            attribution.message
        );
    }

    /// `--plugin-name` is the crate name, but routes are attributed to
    /// `Plugin::name()` (by default the type path). The contract links the
    /// two, so the route checks run under the registered name, and the report
    /// keeps the crate name the index lists.
    /// Routes under both linked names: the checks would read one set and
    /// miss the other, so attribution fails.
    #[test]
    fn routes_split_across_both_names_fail_attribution() {
        let mut contract = demo_contract();
        contract.registered_as = Some("demo_crate::DemoPlugin".to_owned());
        let dump = present(vec![contract]);
        let mut hidden = route();
        hidden.source = "plugin:demo_crate::DemoPlugin".to_owned();
        hidden.path = "/off-prefix".to_owned();
        let report = build_report(&opts(&dump), &[route(), hidden]);
        let attribution = find(&report, "route-attribution");
        assert_eq!(attribution.status, CheckStatus::Fail);
        assert!(
            attribution.message.contains("demo_crate::DemoPlugin"),
            "{}",
            attribution.message
        );
        assert!(!report.passed());
    }

    #[test]
    fn route_checks_follow_the_registered_name() {
        let mut contract = demo_contract();
        contract.registered_as = Some("demo_crate::DemoPlugin".to_owned());
        let dump = present(vec![contract]);
        let mut hidden = route();
        hidden.source = "plugin:demo_crate::DemoPlugin".to_owned();
        let mut o = opts(&dump);
        o.expected_prefix = Some("/elsewhere");
        let report = build_report(&o, &[hidden]);
        assert_eq!(report.report.plugin_name, "autumn-plugin-demo");
        let attribution = find(&report, "route-attribution");
        assert_eq!(
            attribution.status,
            CheckStatus::Pass,
            "{}",
            attribution.message
        );
        assert!(
            attribution.message.contains("demo_crate::DemoPlugin"),
            "{}",
            attribution.message
        );
        // The prefix check ran over those routes, and caught the mismatch.
        assert_eq!(find(&report, "route-prefix").status, CheckStatus::Fail);
    }

    /// `--prefix` declares a routed plugin: no attributed routes is a
    /// failure, never a skip, so the report checks at least one endpoint.
    #[test]
    fn a_registered_plugin_with_a_prefix_and_no_routes_fails() {
        let dump = present(vec![demo_contract()]);
        let mut o = opts(&dump);
        o.expected_prefix = Some("/demo");
        let report = build_report(&o, &[]);
        assert_eq!(find(&report, "route-attribution").status, CheckStatus::Fail);
        assert!(!report.passed());
    }

    /// A contract with no registered name cannot prove the plugin has no
    /// routes under some other name.
    #[test]
    fn a_contract_without_a_registered_name_does_not_skip() {
        let mut contract = demo_contract();
        contract.registered_as = None;
        let dump = present(vec![contract]);
        let report = build_report(&opts(&dump), &[]);
        assert_eq!(find(&report, "route-attribution").status, CheckStatus::Fail);
        // `--no-routes` cannot skip without the registered name either.
        let mut o = opts(&dump);
        o.no_routes = true;
        let report = build_report(&o, &[]);
        assert_eq!(find(&report, "route-attribution").status, CheckStatus::Fail);
    }

    /// Without a matching contract, no routes still means a wrong name.
    #[test]
    fn an_unregistered_name_with_no_routes_still_fails_route_attribution() {
        for dump in [
            ContractDump::Absent,
            present(vec![]),
            present(vec![PluginContract::new("someone-else").autumn_web("0.7")]),
        ] {
            let report = build_report(&opts(&dump), &[]);
            assert_eq!(
                find(&report, "route-attribution").status,
                CheckStatus::Fail,
                "{dump:?}"
            );
        }
    }

    /// The plugin index records the range and tier from the report, so the
    /// report carries the contract it checked.
    #[test]
    fn the_report_carries_the_declared_contract() {
        let dump = present(vec![demo_contract()]);
        let report = build_report(&opts(&dump), &[route()]);
        assert_eq!(report.contract, Some(demo_contract()));
        let json = serde_json::to_value(&report).expect("json");
        assert_eq!(json["contract"]["autumn_web"], "0.7");

        let report = build_report(&opts(&ContractDump::Absent), &[route()]);
        assert_eq!(report.contract, None);
        let json = serde_json::to_value(&report).expect("json");
        assert!(json.get("contract").is_none(), "{json}");
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
