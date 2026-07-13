//! `autumn routes audit` -- prove route authentication coverage at build time.
//!
//! Runs the same dump-routes pipeline as [`crate::routes`], reads the
//! build-time security classification the framework attaches to every route
//! (derived from macro-expanded `#[secured]` / `#[authorize]` / `#[public]`
//! posture), and emits a stable-ordered, machine-readable security manifest.
//!
//! The command is a CI gate: it exits non-zero when any route is
//! `Unclassified` — i.e. a route that is neither framework-owned, guarded, nor
//! explicitly declared public — naming each offending route so the gap can be
//! closed by adding a guard or a `#[public]` marker.
//!
//! This is the first *dimension* of the security manifest described in issue
//! #1604; the `provenance: "provable"` tag on each classification lets #1627
//! grow additional dimensions without breaking the schema.

use std::process::Command;

use autumn_web::route_listing::OMITTED_ROUTES_MARKER;
use serde::{Deserialize, Serialize};

use crate::routes;

/// Schema version of the emitted manifest. Bumped only on breaking changes to
/// the document shape.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Options controlling `autumn routes audit`.
pub struct AuditOptions<'a> {
    pub package: Option<&'a str>,
    /// Binary target name for packages that expose multiple bin targets.
    pub bin: Option<&'a str>,
    /// Write the JSON manifest to this file path (in addition to any stdout).
    pub manifest: Option<&'a str>,
    /// Emit the JSON manifest to stdout instead of the human report.
    pub json: bool,
    /// Reserved: tighten the gate in future revisions. The default posture
    /// (fail on any unclassified route) already applies without it.
    pub strict: bool,
}

/// A single route as read back from the app's dumped route listing.
///
/// The dumped JSON carries more than this (versioning, middleware, …); only the
/// fields relevant to the security manifest are deserialized here. Unknown
/// fields are ignored, and missing ones fall back to their defaults so the audit
/// stays forward/backward compatible with the dump format.
#[derive(Debug, Clone, Deserialize)]
pub struct AuditRoute {
    pub method: String,
    pub path: String,
    /// Handler function name (serialized as `handler` in the dump).
    pub handler: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub classification: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub policy: bool,
    #[serde(default)]
    pub module: Option<String>,
}

impl AuditRoute {
    /// Whether this route lacks a proven security posture. Anything that is not
    /// framework-owned, guarded, or explicitly public fails the gate — including
    /// an empty/unknown classification from an older dump.
    #[must_use]
    pub fn is_unclassified(&self) -> bool {
        !matches!(
            self.classification.as_str(),
            "framework" | "gated" | "public"
        )
    }
}

// ── Manifest document ───────────────────────────────────────────────────────

/// Top-level security manifest.
#[derive(Debug, Serialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub dimensions: Dimensions,
}

/// Manifest dimensions. Only `routes` exists today; additional provable
/// dimensions can be added here without breaking existing consumers.
#[derive(Debug, Serialize)]
pub struct Dimensions {
    pub routes: Vec<ManifestRoute>,
}

/// One route entry in the manifest's `routes` dimension.
#[derive(Debug, Serialize)]
pub struct ManifestRoute {
    pub path: String,
    pub method: String,
    pub name: String,
    pub classification: String,
    pub roles: Vec<String>,
    pub scopes: Vec<String>,
    pub policy: bool,
    pub source: String,
    /// How the classification was established. `"provable"` means it was
    /// derived from macro-expanded code (route + auth posture).
    pub provenance: &'static str,
}

/// Build a stable-ordered security manifest from the audited routes.
///
/// Routes are ordered by `(path, method)` so the manifest is diff-friendly and
/// reproducible across runs.
#[must_use]
pub fn build_manifest(routes: &[AuditRoute]) -> Manifest {
    let mut entries: Vec<ManifestRoute> = routes
        .iter()
        .map(|r| ManifestRoute {
            path: r.path.clone(),
            method: r.method.clone(),
            name: r.handler.clone(),
            classification: r.classification.clone(),
            roles: r.roles.clone(),
            scopes: r.scopes.clone(),
            policy: r.policy,
            source: r.source.clone(),
            provenance: "provable",
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));

    Manifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        dimensions: Dimensions { routes: entries },
    }
}

/// Serialize a manifest to pretty JSON.
#[must_use]
pub fn manifest_json(manifest: &Manifest) -> String {
    serde_json::to_string_pretty(manifest).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

/// The subset of routes that fail the audit gate.
#[must_use]
pub fn unclassified_routes(routes: &[AuditRoute]) -> Vec<&AuditRoute> {
    routes.iter().filter(|r| r.is_unclassified()).collect()
}

/// Exit code for the gate: `0` when every route is classified, `1` when at
/// least one route is unclassified.
#[must_use]
pub fn audit_exit_code(routes: &[AuditRoute]) -> i32 {
    i32::from(routes.iter().any(AuditRoute::is_unclassified))
}

/// Number of raw routers the dumped listing omitted, as reported by the app's
/// `AUTUMN_DUMP_ROUTES` stderr marker ([`OMITTED_ROUTES_MARKER`]).
///
/// Routers added via `AppBuilder::merge()`/`nest()` are opaque and cannot be
/// enumerated, so they never appear in the parsed stdout listing. Their auth
/// posture is therefore unprovable — the audit gate must fail rather than emit a
/// manifest that silently drops them.
#[must_use]
pub fn parse_omitted_count(stderr: &str) -> usize {
    let marker = OMITTED_ROUTES_MARKER.trim();
    // Take the last matching marker line; the dump emits at most one per run.
    stderr
        .lines()
        .filter_map(|line| line.trim().strip_prefix(marker))
        .filter_map(|rest| rest.trim().parse::<usize>().ok())
        .next_back()
        .unwrap_or(0)
}

/// Hard-failure diagnostic for omitted (unenumerable) raw routers. These defeat
/// the coverage proof, so the gate fails and tells the user how to make the
/// routes provable.
#[must_use]
pub fn format_omitted_diagnostic(count: usize) -> String {
    format!(
        "\u{2717} {count} raw router(s) added via `AppBuilder::merge()`/`nest()` \
         are not enumerable and were omitted from the route listing.\n\
         Route auth coverage can't be proven while these exist. Mount routes via \
         `routes![]` (or a plugin's `declare_plugin_routes`) so they are visible \
         and classifiable.\n"
    )
}

/// Human diagnostic naming every unclassified route (method + path + handler,
/// with the handler's module when known).
#[must_use]
pub fn format_diagnostic(unresolved: &[&AuditRoute]) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "\u{2717} {} route(s) have no proven auth posture:",
        unresolved.len()
    );
    for r in unresolved {
        let module = r
            .module
            .as_deref()
            .map(|m| format!(" [{m}]"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  {method:<6} {path}  (handler `{handler}`{module})",
            method = r.method,
            path = r.path,
            handler = r.handler,
        );
    }
    out.push_str(
        "\nAdd a guard (`#[secured]` / `#[authorize]`) or mark the route \
         deliberately open with `#[public]`.\n",
    );
    out
}

/// One-line-per-bucket summary of the classification breakdown.
#[must_use]
pub fn format_summary(routes: &[AuditRoute]) -> String {
    let mut framework = 0usize;
    let mut gated = 0usize;
    let mut public = 0usize;
    let mut unclassified = 0usize;
    for r in routes {
        match r.classification.as_str() {
            "framework" => framework += 1,
            "gated" => gated += 1,
            "public" => public += 1,
            _ => unclassified += 1,
        }
    }
    format!(
        "{total} route(s): {gated} gated, {public} public, {framework} framework, \
         {unclassified} unclassified",
        total = routes.len(),
    )
}

/// Run `autumn routes audit`.
pub fn run(opts: &AuditOptions<'_>) {
    eprintln!("\u{1F342} autumn routes audit\n");
    routes::compile_binary(opts.package, opts.bin);
    let binary = routes::find_binary(opts.package, opts.bin);

    // Capture stderr (rather than inheriting it) so we can detect the app's
    // omitted-routes marker; forward it verbatim afterwards so warnings stay
    // visible to the user.
    let output = Command::new(&binary)
        .env("AUTUMN_DUMP_ROUTES", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    let stderr = String::from_utf8_lossy(&output.stderr);
    eprint!("{stderr}");
    let omitted = parse_omitted_count(&stderr);

    if !output.status.success() {
        eprintln!(
            "\u{2717} Binary exited with status {} while dumping routes",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let routes: Vec<AuditRoute> = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        eprintln!("\u{2717} Failed to parse route listing JSON: {e}");
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    });

    let manifest = build_manifest(&routes);
    let json = manifest_json(&manifest);

    if let Some(path) = opts.manifest {
        if let Err(e) = std::fs::write(path, format!("{json}\n")) {
            eprintln!("\u{2717} Failed to write manifest to {path}: {e}");
            std::process::exit(1);
        }
        eprintln!("\u{2713} Wrote security manifest \u{2192} {path}");
    }

    if opts.json {
        println!("{json}");
    } else {
        println!("{}", format_summary(&routes));
    }

    let unresolved = unclassified_routes(&routes);
    if unresolved.is_empty() {
        if !opts.json && omitted == 0 {
            eprintln!("\u{2713} All routes are classified.");
        }
    } else {
        eprint!("\n{}", format_diagnostic(&unresolved));
        if opts.strict {
            eprintln!("(strict mode)");
        }
    }

    if omitted > 0 {
        eprint!("\n{}", format_omitted_diagnostic(omitted));
    }

    // Fail the gate on either unclassified routes or omitted (unprovable) ones.
    let failed = omitted > 0 || audit_exit_code(&routes) != 0;
    std::process::exit(i32::from(failed));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(method: &str, path: &str, handler: &str, classification: &str) -> AuditRoute {
        AuditRoute {
            method: method.to_owned(),
            path: path.to_owned(),
            handler: handler.to_owned(),
            source: "user".to_owned(),
            classification: classification.to_owned(),
            roles: vec![],
            scopes: vec![],
            policy: false,
            module: None,
        }
    }

    // ── classification / gate ────────────────────────────────────────────────

    #[test]
    fn is_unclassified_true_for_unclassified_and_unknown() {
        assert!(route("POST", "/w", "create", "unclassified").is_unclassified());
        assert!(route("POST", "/w", "create", "").is_unclassified());
        assert!(route("POST", "/w", "create", "bogus").is_unclassified());
    }

    #[test]
    fn is_unclassified_false_for_known_postures() {
        assert!(!route("GET", "/w", "list", "gated").is_unclassified());
        assert!(!route("GET", "/w", "list", "public").is_unclassified());
        assert!(!route("GET", "/health", "health", "framework").is_unclassified());
    }

    #[test]
    fn exit_code_is_nonzero_iff_any_route_unclassified() {
        let clean = vec![
            route("GET", "/a", "a", "public"),
            route("GET", "/b", "b", "gated"),
            route("GET", "/health", "health", "framework"),
        ];
        assert_eq!(audit_exit_code(&clean), 0);

        let mut dirty = clean;
        dirty.push(route("POST", "/widgets", "create_widget", "unclassified"));
        assert_eq!(audit_exit_code(&dirty), 1);
    }

    #[test]
    fn unclassified_routes_returns_only_the_offenders() {
        let routes = vec![
            route("GET", "/a", "a", "public"),
            route("POST", "/widgets", "create_widget", "unclassified"),
            route("DELETE", "/things/{id}", "delete_thing", ""),
        ];
        let unresolved = unclassified_routes(&routes);
        assert_eq!(unresolved.len(), 2);
        assert!(unresolved.iter().any(|r| r.handler == "create_widget"));
        assert!(unresolved.iter().any(|r| r.handler == "delete_thing"));
    }

    // ── diagnostic ───────────────────────────────────────────────────────────

    #[test]
    fn diagnostic_names_the_offending_route() {
        let mut r = route("POST", "/widgets", "create_widget", "unclassified");
        r.module = Some("myapp::widgets".to_owned());
        let unresolved = vec![&r];
        let msg = format_diagnostic(&unresolved);
        assert!(msg.contains("POST"), "{msg}");
        assert!(msg.contains("/widgets"), "{msg}");
        assert!(msg.contains("create_widget"), "{msg}");
        assert!(msg.contains("myapp::widgets"), "{msg}");
        assert!(msg.contains("#[public]"), "{msg}");
    }

    // ── manifest shape / stability ───────────────────────────────────────────

    #[test]
    fn manifest_is_schema_shaped_and_provable() {
        let routes = vec![route("GET", "/a", "a", "gated")];
        let manifest = build_manifest(&routes);
        assert_eq!(manifest.schema_version, MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest.dimensions.routes.len(), 1);
        assert_eq!(manifest.dimensions.routes[0].provenance, "provable");

        let json: serde_json::Value = serde_json::from_str(&manifest_json(&manifest)).unwrap();
        assert_eq!(json["schema_version"], 1);
        let entry = &json["dimensions"]["routes"][0];
        for key in [
            "path",
            "method",
            "name",
            "classification",
            "roles",
            "scopes",
            "policy",
            "source",
            "provenance",
        ] {
            assert!(entry.get(key).is_some(), "manifest route missing `{key}`");
        }
    }

    #[test]
    fn manifest_routes_are_stable_ordered_by_path_then_method() {
        let routes = vec![
            route("POST", "/posts", "create", "gated"),
            route("GET", "/posts", "list", "public"),
            route("GET", "/about", "about", "public"),
        ];
        let manifest = build_manifest(&routes);
        let order: Vec<(&str, &str)> = manifest
            .dimensions
            .routes
            .iter()
            .map(|r| (r.path.as_str(), r.method.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![("/about", "GET"), ("/posts", "GET"), ("/posts", "POST")]
        );
    }

    #[test]
    fn manifest_carries_roles_scopes_policy() {
        let mut r = route("POST", "/admin", "admin_action", "gated");
        r.roles = vec!["admin".to_owned()];
        r.scopes = vec!["posts:write".to_owned()];
        r.policy = true;
        let manifest = build_manifest(&[r]);
        let entry = &manifest.dimensions.routes[0];
        assert_eq!(entry.roles, vec!["admin"]);
        assert_eq!(entry.scopes, vec!["posts:write"]);
        assert!(entry.policy);
    }

    // ── falsifiability (#1604): red → green over the parsed dump ──────────────

    /// A dumped listing containing one deliberately-unannotated mutating
    /// handler fails the gate and names it; re-classifying that route as either
    /// `gated` (guarded) or `public` turns the gate green.
    #[test]
    fn audit_gate_red_then_green() {
        let dump = |cls: &str| {
            vec![
                route("GET", "/health", "health", "framework"),
                route("POST", "/widgets", "create_widget", cls),
            ]
        };

        // Red: the mutating route is unclassified.
        let red = dump("unclassified");
        assert_eq!(audit_exit_code(&red), 1);
        let unresolved = unclassified_routes(&red);
        assert_eq!(unresolved.len(), 1);
        assert!(format_diagnostic(&unresolved).contains("create_widget"));

        // Green via a guard.
        assert_eq!(audit_exit_code(&dump("gated")), 0);
        // Green via #[public].
        assert_eq!(audit_exit_code(&dump("public")), 0);
    }

    // ── omitted routes (raw merge/nest routers) ──────────────────────────────

    #[test]
    fn parse_omitted_count_reads_the_marker() {
        let stderr = format!(
            "\u{1F342} autumn routes\n\
             [autumn routes] warning: 2 raw router(s) added via .merge()/.nest() ...\n\
             {OMITTED_ROUTES_MARKER}2\n"
        );
        assert_eq!(parse_omitted_count(&stderr), 2);
    }

    #[test]
    fn parse_omitted_count_zero_when_no_marker() {
        assert_eq!(parse_omitted_count(""), 0);
        assert_eq!(parse_omitted_count("some unrelated warning\n"), 0);
    }

    /// A dump that omits raw routers (marker present, count > 0) must fail the
    /// gate even when every *visible* route is fully classified — the omitted
    /// routes are unprovable, so the audit can't pass silently.
    #[test]
    fn omitted_routes_fail_the_gate_even_when_visible_routes_are_clean() {
        // All visible routes are classified: audit_exit_code alone would pass.
        let visible = vec![
            route("GET", "/health", "health", "framework"),
            route("GET", "/posts", "list", "public"),
        ];
        assert_eq!(audit_exit_code(&visible), 0);

        // But the child reported an omitted raw router on stderr.
        let stderr = format!("{OMITTED_ROUTES_MARKER}1\n");
        let omitted = parse_omitted_count(&stderr);
        assert_eq!(omitted, 1);

        // The combined gate (mirroring `run`) must fail.
        let failed = omitted > 0 || audit_exit_code(&visible) != 0;
        assert!(failed, "omitted routes must hard-fail the audit gate");

        // And the diagnostic explains why and how to fix it.
        let diag = format_omitted_diagnostic(omitted);
        assert!(diag.contains("merge()"), "{diag}");
        assert!(diag.contains("routes!["), "{diag}");
    }

    /// The mirror of the case above (#1604): a raw router mounted with covering
    /// declarations — as the first-party `AdminPlugin` does via
    /// `nest_declared` — is enumerable, so the app emits NO omitted marker
    /// (`hidden == 0`). `parse_omitted_count` returns 0 and, with clean visible
    /// routes, the combined gate must PASS. Previously the nested admin router
    /// still tripped the marker and false-failed the audit.
    #[test]
    fn declared_nest_emits_no_marker_and_passes_the_gate() {
        // Visible routes include the declared admin endpoints, all classified.
        let visible = vec![
            route("GET", "/health", "health", "framework"),
            route("GET", "/admin", "admin::index", "gated"),
            route("POST", "/admin/users", "admin::create", "gated"),
        ];
        assert_eq!(audit_exit_code(&visible), 0);

        // A declared-covered nest emits no marker, so no omitted count is
        // reported even though a raw router was mounted.
        let stderr = "\u{1F342} autumn routes\n";
        let omitted = parse_omitted_count(stderr);
        assert_eq!(
            omitted, 0,
            "a declared-covered nest must not emit the marker"
        );

        // The combined gate (mirroring `run`) must pass.
        let failed = omitted > 0 || audit_exit_code(&visible) != 0;
        assert!(
            !failed,
            "a declared-covered admin nest must not false-fail the audit gate"
        );
    }
}
