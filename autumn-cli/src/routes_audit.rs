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

use autumn_web::route_listing::{
    AuthorizeBindingInfo, CsrfDump, HeadersDump, OMITTED_ROUTES_MARKER, SECURITY_CONFIG_MARKER,
    SecurityDump,
};
use serde::{Deserialize, Serialize};

use crate::routes;

/// Schema version of the emitted manifest. Bumped only on breaking changes to
/// the document shape.
///
/// v2 (#1627) wrapped each dimension in a provenance envelope
/// (`{ provenance, source, entries }`), added the `csrf` and `security_headers`
/// dimensions, and added a top-level `excluded` list. The `routes` entries keep
/// their v1 shape, only wrapped in the envelope.
///
/// v3 (#1627) promotes `authorization_policies` from `excluded` to a fourth
/// emitted dimension: the route→action→resource bindings proven from expanded
/// `#[authorize]` attributes, in a provenance envelope that carries an extra
/// `runtime_caveat` naming the one part of the story a build cannot prove.
/// `excluded` keeps `policy_registration` for that runtime fact and gains
/// `repository_policy_bindings` for the auto-API guards whose policy type the
/// macro discards. The v2 dimensions are unchanged.
pub const MANIFEST_SCHEMA_VERSION: u32 = 3;

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
    /// `file:line` source location of the handler, when known (serialized as
    /// `location` in the dump).
    #[serde(default)]
    pub location: Option<String>,
    /// Record-level `(action, resource)` bindings proven from the handler's
    /// `#[authorize]` attributes, already sorted and deduplicated by the dump.
    /// Absent on older dumps and on routes that declare none.
    #[serde(default)]
    pub authorize_bindings: Vec<AuthorizeBindingInfo>,
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

// ── Manifest document (schema v3, #1627) ────────────────────────────────────

/// Provenance class of a manifest dimension. A small closed enum serialized to
/// the exact lowercase tags used throughout the manifest.
///
/// - `provable` — derived from macro-expanded code; the manifest *proves* it.
/// - `declared` — read from configuration; the manifest reports what was
///   *configured*, not that the runtime honors it.
/// - `runtime-only` — a boot/runtime fact that a build-time manifest cannot
///   observe; listed only in `excluded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    Provable,
    Declared,
    #[serde(rename = "runtime-only")]
    RuntimeOnly,
}

/// Top-level security manifest (schema v3).
#[derive(Debug, Serialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub dimensions: Dimensions,
    /// Dimensions deliberately not yet emitted, with the provenance class they
    /// will eventually carry and why they are excluded from this build.
    pub excluded: Vec<ExcludedDimension>,
}

/// Manifest dimensions. Order is fixed (`routes`, then `csrf`, then
/// `security_headers`, then `authorization_policies`) so the serialized
/// document is diff-stable. New dimensions append, keeping the v2 keys in
/// place.
#[derive(Debug, Serialize)]
pub struct Dimensions {
    pub routes: RoutesDimension,
    pub csrf: CsrfDimension,
    pub security_headers: HeadersDimension,
    pub authorization_policies: AuthorizationPoliciesDimension,
}

/// The `routes` dimension: provable auth posture per mounted route.
#[derive(Debug, Serialize)]
pub struct RoutesDimension {
    pub provenance: Provenance,
    pub source: &'static str,
    pub entries: Vec<ManifestRoute>,
}

/// One route entry in the manifest's `routes` dimension. Byte-for-byte
/// identical to the v1 shape (only its container changed).
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
    /// `file:line` source location of the handler, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// How the classification was established. `"provable"` means it was
    /// derived from macro-expanded code (route + auth posture).
    pub provenance: &'static str,
}

/// The `csrf` dimension: declared CSRF enforcement per mutating route.
#[derive(Debug, Serialize)]
pub struct CsrfDimension {
    pub provenance: Provenance,
    pub source: &'static str,
    /// Configured exempt path prefixes (sorted).
    pub exempt_paths: Vec<String>,
    pub entries: Vec<CsrfEntry>,
}

/// One CSRF entry: a single mutating route and whether CSRF is enforced on it.
#[derive(Debug, Serialize)]
pub struct CsrfEntry {
    pub path: String,
    pub method: String,
    /// `enabled && !is_safe(method) && !is_exempt(path)`, mirroring the runtime
    /// `CsrfLayer` predicate.
    pub csrf_enforced: bool,
    /// Whether this route's path matches a configured exemption prefix.
    pub exempt: bool,
}

/// The `security_headers` dimension: declared response security headers.
#[derive(Debug, Serialize)]
pub struct HeadersDimension {
    pub provenance: Provenance,
    pub source: &'static str,
    pub entries: Vec<HeaderEntry>,
}

/// One security-header entry. `emitted` is `true` only when `value` is non-empty
/// *and* parses as a valid `HeaderValue`, mirroring the runtime layer which
/// skips headers whose value `HeaderValue::from_str` rejects.
#[derive(Debug, Serialize)]
pub struct HeaderEntry {
    pub header: String,
    pub value: String,
    pub emitted: bool,
}

/// The `authorization_policies` dimension: provable record-level authorization
/// bindings, one entry per route × `#[authorize]` binding.
///
/// Provable, because every entry is recovered from macro-expanded code — but
/// only as far as the resource *identifier*. Which `impl Policy<Resource>`
/// answers the check is a boot fact, disclosed on the dimension itself as
/// [`Self::runtime_caveat`] rather than left for the reader to infer.
#[derive(Debug, Serialize)]
pub struct AuthorizationPoliciesDimension {
    pub provenance: Provenance,
    pub source: &'static str,
    /// The part of the authorization story a build-time manifest cannot prove,
    /// stated on the dimension itself instead of being buried in `excluded`.
    pub runtime_caveat: &'static str,
    pub entries: Vec<AuthorizationPolicyEntry>,
}

/// One authorization binding: a route plus the `(action, resource)` pair one of
/// its `#[authorize]` attributes declares. A route carrying several bindings
/// contributes several entries.
#[derive(Debug, Serialize)]
pub struct AuthorizationPolicyEntry {
    pub path: String,
    pub method: String,
    pub name: String,
    pub action: String,
    pub resource: String,
    /// How the binding was established. `"provable"` means it was derived from
    /// macro-expanded code (route + `#[authorize]` arguments).
    pub provenance: &'static str,
}

/// A dimension deliberately not yet emitted in this manifest build.
#[derive(Debug, Serialize)]
pub struct ExcludedDimension {
    pub dimension: &'static str,
    pub eventual_provenance: Provenance,
    pub reason: &'static str,
}

/// Normalize a route's *listed* method to the method the runtime router actually
/// mounts and dispatches on.
///
/// `#[ws]` routes are listed with the synthetic method `WS`, but the macro mounts
/// them via `axum::routing::get` (see `autumn-macros/src/ws.rs`), so at runtime
/// `CsrfService` sees a real `GET` and checks *that* against the configured
/// `safe_methods` (`autumn/src/security/csrf.rs`). Deciding CSRF safety on the
/// normalized `GET` keeps the manifest faithful to runtime in both the common
/// case (default config: `GET` is safe → WS never CSRF-validated) and the
/// customized case (an app removing `GET` from `safe_methods` → the WS upgrade,
/// being a GET, *is* CSRF-validated and must show up as enforced).
fn normalize_method(method: &str) -> &str {
    if method == "WS" { "GET" } else { method }
}

/// Whether `method` is in the configured safe-method set (case-sensitive; route
/// methods and config defaults are both upper-case). The listed method is first
/// normalized to what the router mounts (see [`normalize_method`]) so the
/// safe/exempt decision matches runtime, which only ever sees the mounted method.
fn method_is_safe(method: &str, safe_methods: &[String]) -> bool {
    let method = normalize_method(method);
    safe_methods.iter().any(|m| m == method)
}

/// Whether `path` matches one of the configured CSRF exemption prefixes.
///
/// Mirrors the runtime `CsrfLayer` exemption predicate
/// (`autumn/src/security/csrf.rs`): a prefix matches on an exact equality, or a
/// prefix match whose boundary is a `/` (either the prefix ends with `/`, or the
/// remainder starts with `/`). Route paths in the manifest are already
/// normalized, so no dot-segment cleaning is required here.
fn path_is_exempt(path: &str, exempt_paths: &[String]) -> bool {
    exempt_paths.iter().any(|prefix| {
        if path == prefix {
            true
        } else if let Some(stripped) = path.strip_prefix(prefix.as_str()) {
            prefix.ends_with('/') || stripped.starts_with('/')
        } else {
            false
        }
    })
}

/// Build the `routes` dimension (provable) from the audited routes, ordered by
/// `(path, method)`.
fn build_routes_dimension(routes: &[AuditRoute]) -> RoutesDimension {
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
            location: r.location.clone(),
            provenance: "provable",
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));
    RoutesDimension {
        provenance: Provenance::Provable,
        source: "macro:#[secured]/#[authorize]/#[public]",
        entries,
    }
}

/// Build the `csrf` dimension (declared): one entry per *mutating* route (method
/// not in `safe_methods`), plus the sorted configured exemptions.
fn build_csrf_dimension(routes: &[AuditRoute], csrf: &CsrfDump) -> CsrfDimension {
    let mut exempt_paths = csrf.exempt_paths.clone();
    exempt_paths.sort();
    exempt_paths.dedup();

    let mut entries: Vec<CsrfEntry> = routes
        .iter()
        .filter(|r| !method_is_safe(&r.method, &csrf.safe_methods))
        .map(|r| {
            let exempt = path_is_exempt(&r.path, &exempt_paths);
            CsrfEntry {
                path: r.path.clone(),
                method: r.method.clone(),
                // Mutating method here, so `is_safe` reduces to `is_exempt`.
                csrf_enforced: csrf.enabled && !exempt,
                exempt,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));

    CsrfDimension {
        provenance: Provenance::Declared,
        source: "config:security.csrf",
        exempt_paths,
        entries,
    }
}

/// Build the `security_headers` dimension (declared): one entry per header key,
/// in sorted key order. `value` is the effective declared string (empty when
/// the header is not emitted).
fn build_headers_dimension(h: &HeadersDump) -> HeadersDimension {
    let hsts_value = if h.strict_transport_security {
        let mut v = format!("max-age={}", h.hsts_max_age_secs);
        if h.hsts_include_subdomains {
            v.push_str("; includeSubDomains");
        }
        v
    } else {
        String::new()
    };
    let bool_header = |on: bool, val: &str| if on { val.to_owned() } else { String::new() };

    let mut entries = vec![
        header_entry("content_security_policy", h.content_security_policy.clone()),
        header_entry("permissions_policy", h.permissions_policy.clone()),
        header_entry("referrer_policy", h.referrer_policy.clone()),
        header_entry("strict_transport_security", hsts_value),
        header_entry(
            "x_content_type_options",
            bool_header(h.x_content_type_options, "nosniff"),
        ),
        header_entry("x_frame_options", h.x_frame_options.clone()),
        header_entry(
            "xss_protection",
            bool_header(h.xss_protection, "1; mode=block"),
        ),
    ];
    entries.sort_by(|a, b| a.header.cmp(&b.header));

    HeadersDimension {
        provenance: Provenance::Declared,
        source: "config:security.headers",
        entries,
    }
}

fn header_entry(header: &str, value: String) -> HeaderEntry {
    // Mirror the runtime security-headers layer, which inserts a header only
    // when `HeaderValue::from_str(value)` succeeds and otherwise skips it. A
    // non-empty but invalid value (e.g. one containing a newline) is therefore
    // NOT emitted — the manifest must expose that misconfiguration, not paper
    // over it by reporting `emitted: true`.
    use autumn_web::reexports::http::HeaderValue;
    let emitted = !value.is_empty() && HeaderValue::from_str(&value).is_ok();
    HeaderEntry {
        header: header.to_owned(),
        emitted,
        value,
    }
}

/// Build the `authorization_policies` dimension (provable): one flat entry per
/// route × `#[authorize]` binding, ordered by `(path, method, action,
/// resource)`.
///
/// Framework routes are skipped. The framework mounts them itself, so they can
/// carry no `#[authorize]`; `route_listing::authorize_bindings_of` already
/// drops any binding on them, and repeating the rule here keeps a malformed
/// dump from inventing an authorization claim the code never made.
///
/// A route with `policy: true` but no bindings contributes nothing: that
/// boolean is a superset also covering inline `__check_policy` calls and
/// `#[repository(policy = ...)]` auto-APIs, neither of which proves an
/// `(action, resource)` pair (see the `repository_policy_bindings` entry in
/// [`excluded_dimensions`]).
fn build_authorization_policies_dimension(routes: &[AuditRoute]) -> AuthorizationPoliciesDimension {
    let mut entries: Vec<AuthorizationPolicyEntry> = routes
        .iter()
        .filter(|r| r.source != "framework")
        .flat_map(|r| {
            r.authorize_bindings
                .iter()
                .map(move |b| AuthorizationPolicyEntry {
                    path: r.path.clone(),
                    method: r.method.clone(),
                    name: r.handler.clone(),
                    action: b.action.clone(),
                    resource: b.resource.clone(),
                    provenance: "provable",
                })
        })
        .collect();
    entries.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.method.cmp(&b.method))
            .then_with(|| a.action.cmp(&b.action))
            .then_with(|| a.resource.cmp(&b.resource))
    });

    AuthorizationPoliciesDimension {
        provenance: Provenance::Provable,
        source: "macro:#[authorize]",
        runtime_caveat: "the route→action→resource binding is proven from macro-expanded code; \
                         which impl Policy<Resource> serves it is resolved from the \
                         PolicyRegistry at boot (AppBuilder::policy::<R, _>(...)). A missing \
                         registration is not visible at build time — see the policy_registration \
                         entry in `excluded`.",
        entries,
    }
}

/// The fixed, deterministically-ordered list of dimensions deliberately
/// excluded from this manifest build (#1627).
fn excluded_dimensions() -> Vec<ExcludedDimension> {
    vec![
        ExcludedDimension {
            dimension: "repository_policy_bindings",
            eventual_provenance: Provenance::Provable,
            reason: "#[repository(api = ..., policy = ...)] auto-API handlers enforce fixed \
                     show/create/update/delete policy actions, but the macro discards the policy \
                     type at expansion and RepositoryApiMeta carries only a type-erased probe; \
                     their presence still shows as routes.entries[].policy",
        },
        ExcludedDimension {
            dimension: "sessions",
            eventual_provenance: Provenance::Declared,
            reason: "later phase",
        },
        ExcludedDimension {
            dimension: "secrets",
            eventual_provenance: Provenance::Declared,
            reason: "later phase",
        },
        ExcludedDimension {
            dimension: "encryption",
            eventual_provenance: Provenance::Provable,
            reason: "later phase (#[encrypted] fields provable; key config declared)",
        },
        ExcludedDimension {
            dimension: "rate_limit",
            eventual_provenance: Provenance::Declared,
            reason: "later phase",
        },
        ExcludedDimension {
            dimension: "submit_token",
            eventual_provenance: Provenance::Declared,
            reason: "later phase",
        },
        ExcludedDimension {
            dimension: "outbound_http",
            eventual_provenance: Provenance::Declared,
            reason: "later phase; named-client base_urls allowlist is config, nothing proves every \
                     outbound call routes through a named client",
        },
        ExcludedDimension {
            dimension: "policy_registration",
            eventual_provenance: Provenance::RuntimeOnly,
            reason: "boot-time fact; excluded from a build-time manifest by design — the \
                     authorization_policies dimension proves bindings and carries this as its \
                     runtime_caveat",
        },
        ExcludedDimension {
            dimension: "serve_path_routers",
            // Provable once plumbed: these are real mounted routes that
            // `autumn routes` would enumerate (like the routes dimension) if
            // the dump path injected them into merge_routers.
            eventual_provenance: Provenance::Provable,
            reason: "opt-in serve-path HTTP surfaces (MCP, inbound-mail, storage, SEO) are \
                     injected only on the serve path, after the dump early-exit, so they are not \
                     enumerated by `autumn routes` at build time; their mutating POSTs (MCP, \
                     inbound-mail) run outside the framework CSRF layer and self-protect (MCP: \
                     origin + csrf_header + trusted-host; inbound-mail: provider signature \
                     verification). Enumerating them is deferred to a dump-plumbing follow-up",
        },
    ]
}

/// The `csrf` dimension emitted when no security config was reported.
const fn empty_csrf_dimension() -> CsrfDimension {
    CsrfDimension {
        provenance: Provenance::Declared,
        source: "config:security.csrf",
        exempt_paths: Vec::new(),
        entries: Vec::new(),
    }
}

/// The `security_headers` dimension emitted when no security config was reported.
const fn empty_headers_dimension() -> HeadersDimension {
    HeadersDimension {
        provenance: Provenance::Declared,
        source: "config:security.headers",
        entries: Vec::new(),
    }
}

/// Build a stable-ordered security manifest (schema v3) from the audited routes
/// and the resolved security configuration.
///
/// When `security` is `None` (an older dump with no security-config marker) the
/// declared dimensions are emitted empty rather than fabricated, keeping the
/// schema stable without asserting configuration the dump did not report. The
/// provable dimensions (`routes`, `authorization_policies`) read the dumped
/// listing alone, so they are identical either way.
///
/// All dimensions are deterministically ordered so the manifest is diff-friendly
/// and reproducible across runs.
#[must_use]
pub fn build_manifest(routes: &[AuditRoute], security: Option<&SecurityDump>) -> Manifest {
    let routes_dim = build_routes_dimension(routes);
    let (csrf, security_headers) = security.map_or_else(
        || (empty_csrf_dimension(), empty_headers_dimension()),
        |s| {
            (
                build_csrf_dimension(routes, &s.csrf),
                build_headers_dimension(&s.headers),
            )
        },
    );

    Manifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        dimensions: Dimensions {
            routes: routes_dim,
            csrf,
            security_headers,
            authorization_policies: build_authorization_policies_dimension(routes),
        },
        excluded: excluded_dimensions(),
    }
}

/// Parse the security-config marker line ([`SECURITY_CONFIG_MARKER`]) from the
/// child's stderr, if present. Takes the last matching line (the dump emits at
/// most one per run).
#[must_use]
pub fn parse_security_config(stderr: &str) -> Option<SecurityDump> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix(SECURITY_CONFIG_MARKER))
        .filter_map(|json| serde_json::from_str::<SecurityDump>(json.trim()).ok())
        .next_back()
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
        let location = r
            .location
            .as_deref()
            .map(|l| format!(" at {l}"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  {method:<6} {path}  (handler `{handler}`{module}{location})",
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
        // Ask the app to also dump its resolved security config (CSRF + headers)
        // so we can build the `declared` manifest dimensions. Only `audit` sets
        // this; plain `autumn routes` leaves the marker off its stderr.
        .env("AUTUMN_DUMP_SECURITY", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Forward the child's stderr verbatim, but hide the machine-readable
    // security-config marker line (it carries a full JSON blob for us, not the
    // user).
    for line in stderr.lines() {
        if !line.starts_with(SECURITY_CONFIG_MARKER) {
            eprintln!("{line}");
        }
    }
    let omitted = parse_omitted_count(&stderr);
    let security = parse_security_config(&stderr);

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

    let manifest = build_manifest(&routes, security.as_ref());
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
            location: None,
            authorize_bindings: Vec::new(),
        }
    }

    /// A route guarded by one or more `#[authorize]` attributes: the dump
    /// carries the `(action, resource)` pairs *and* sets `policy: true`, since
    /// the expanded guard is a policy check like any other.
    fn route_with_bindings(
        method: &str,
        path: &str,
        handler: &str,
        classification: &str,
        bindings: &[(&str, &str)],
    ) -> AuditRoute {
        let mut r = route(method, path, handler, classification);
        r.policy = true;
        r.authorize_bindings = bindings
            .iter()
            .map(|(action, resource)| AuthorizeBindingInfo {
                action: (*action).to_owned(),
                resource: (*resource).to_owned(),
            })
            .collect();
        r
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

    /// (#1604 deferred item) when the dump carries a `file!()`/`line!()`
    /// source location, the diagnostic names it so an offending handler can
    /// be jumped to directly instead of only naming its module.
    #[test]
    fn diagnostic_includes_source_location_when_known() {
        let mut r = route("POST", "/widgets", "create_widget", "unclassified");
        r.location = Some("src/routes/widgets.rs:42".to_owned());
        let unresolved = vec![&r];
        let msg = format_diagnostic(&unresolved);
        assert!(msg.contains("src/routes/widgets.rs:42"), "{msg}");
    }

    /// A route dumped without a location (older binary, or a route built
    /// without the route macros) must not print a stray `at ` fragment.
    #[test]
    fn diagnostic_omits_location_when_unknown() {
        let r = route("POST", "/widgets", "create_widget", "unclassified");
        let unresolved = vec![&r];
        let msg = format_diagnostic(&unresolved);
        assert!(!msg.contains(" at "), "{msg}");
    }

    // ── manifest shape / stability ───────────────────────────────────────────

    #[test]
    fn manifest_is_schema_shaped_and_provable() {
        let routes = vec![route("GET", "/a", "a", "gated")];
        let manifest = build_manifest(&routes, None);
        assert_eq!(manifest.schema_version, MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest.dimensions.routes.entries.len(), 1);
        assert_eq!(manifest.dimensions.routes.entries[0].provenance, "provable");

        let json: serde_json::Value = serde_json::from_str(&manifest_json(&manifest)).unwrap();
        assert_eq!(json["schema_version"], 3);
        let entry = &json["dimensions"]["routes"]["entries"][0];
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
        let manifest = build_manifest(&routes, None);
        let order: Vec<(&str, &str)> = manifest
            .dimensions
            .routes
            .entries
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
        let manifest = build_manifest(&[r], None);
        let entry = &manifest.dimensions.routes.entries[0];
        assert_eq!(entry.roles, vec!["admin"]);
        assert_eq!(entry.scopes, vec!["posts:write"]);
        assert!(entry.policy);
    }

    // ── provenance envelope + declared dimensions (#1627) ────────────────────

    /// Build a [`SecurityDump`] mirroring the app's `SecurityDump::from_config`
    /// wire snapshot, with sensible defaults callers can override per-field.
    fn security_dump(enabled: bool, exempt_paths: &[&str]) -> SecurityDump {
        SecurityDump {
            csrf: CsrfDump {
                enabled,
                safe_methods: vec![
                    "GET".to_owned(),
                    "HEAD".to_owned(),
                    "OPTIONS".to_owned(),
                    "TRACE".to_owned(),
                ],
                exempt_paths: exempt_paths.iter().map(|s| (*s).to_owned()).collect(),
            },
            headers: HeadersDump {
                x_frame_options: "DENY".to_owned(),
                x_content_type_options: true,
                xss_protection: true,
                content_security_policy: "default-src 'self'".to_owned(),
                referrer_policy: "strict-origin-when-cross-origin".to_owned(),
                permissions_policy: String::new(),
                strict_transport_security: false,
                hsts_max_age_secs: 31_536_000,
                hsts_include_subdomains: true,
                csp_nonce: false,
            },
        }
    }

    /// Serialize a manifest to a `serde_json::Value` for structural comparison.
    fn manifest_value(m: &Manifest) -> serde_json::Value {
        serde_json::from_str(&manifest_json(m)).unwrap()
    }

    /// AC-1: the top-level document is schema v3, carries the four dimensions
    /// with the correct provenance labels, and the `excluded` list with its
    /// closed provenance enum values.
    #[test]
    fn manifest_envelope_shape_and_provenance_labels() {
        let routes = vec![
            route("GET", "/health", "health", "framework"),
            route("POST", "/widgets", "create_widget", "gated"),
        ];
        let sec = security_dump(true, &[]);
        let json = manifest_value(&build_manifest(&routes, Some(&sec)));

        assert_eq!(json["schema_version"], 3);
        assert_eq!(json["dimensions"]["routes"]["provenance"], "provable");
        assert_eq!(
            json["dimensions"]["routes"]["source"],
            "macro:#[secured]/#[authorize]/#[public]"
        );
        assert_eq!(json["dimensions"]["csrf"]["provenance"], "declared");
        assert_eq!(json["dimensions"]["csrf"]["source"], "config:security.csrf");
        assert_eq!(
            json["dimensions"]["security_headers"]["provenance"],
            "declared"
        );
        assert_eq!(
            json["dimensions"]["security_headers"]["source"],
            "config:security.headers"
        );
        assert_eq!(
            json["dimensions"]["authorization_policies"]["provenance"],
            "provable"
        );
        assert_eq!(
            json["dimensions"]["authorization_policies"]["source"],
            "macro:#[authorize]"
        );
        // The provable dimension states its own limit on the wire rather than
        // leaving readers to infer it from `excluded`.
        assert!(
            !json["dimensions"]["authorization_policies"]["runtime_caveat"]
                .as_str()
                .expect("runtime_caveat string")
                .is_empty(),
            "the authorization_policies dimension must carry a non-empty runtime_caveat"
        );

        // Excluded list is present, ordered, and uses the closed enum values.
        let excluded = json["excluded"].as_array().expect("excluded array");
        assert_eq!(excluded.len(), 9);
        assert_eq!(excluded[0]["dimension"], "repository_policy_bindings");
        assert_eq!(excluded[0]["eventual_provenance"], "provable");
        // v3: `authorization_policies` graduated out of `excluded` into a real
        // dimension; leaving a stale excluded entry behind would claim the
        // manifest still cannot prove what it now proves.
        assert!(
            !excluded
                .iter()
                .any(|e| e["dimension"] == "authorization_policies"),
            "authorization_policies is emitted now and must not also be listed as excluded"
        );
        // The registry lookup that resolves a binding to an `impl Policy<_>`
        // stays a boot fact, so `policy_registration` stays excluded.
        let runtime_only = excluded
            .iter()
            .find(|e| e["dimension"] == "policy_registration")
            .expect("policy_registration excluded");
        assert_eq!(runtime_only["eventual_provenance"], "runtime-only");

        // The serve-path routers (MCP, inbound-mail, storage, SEO) are injected
        // only on the serve path (after the dump early-exit), so they are
        // honestly disclosed as excluded rather than silently absent (#1627 AC).
        let serve_path = excluded
            .iter()
            .find(|e| e["dimension"] == "serve_path_routers")
            .expect("serve_path_routers excluded");
        assert_eq!(serve_path["eventual_provenance"], "provable");
    }

    // ── authorization_policies dimension (#1627 slice 2) ─────────────────────

    /// AC-4: an `authorization_policies` entry carries exactly the route
    /// coordinates plus the proven `(action, resource)` pair and its own
    /// provenance tag — no more (the `from = ident` parameter name is a binding
    /// detail, not a security fact) and no less.
    #[test]
    fn manifest_authorization_policies_entry_shape() {
        let routes = vec![route_with_bindings(
            "POST",
            "/notes/{id}",
            "update_note",
            "gated",
            &[("update", "Note")],
        )];
        let json = manifest_value(&build_manifest(&routes, None));
        let entry = &json["dimensions"]["authorization_policies"]["entries"][0];
        let obj = entry.as_object().expect("entry object");

        let keys = ["path", "method", "name", "action", "resource", "provenance"];
        for key in keys {
            assert!(obj.contains_key(key), "entry missing `{key}`: {entry}");
        }
        assert_eq!(obj.len(), keys.len(), "unexpected extra keys: {entry}");

        assert_eq!(entry["path"], "/notes/{id}");
        assert_eq!(entry["method"], "POST");
        assert_eq!(entry["name"], "update_note");
        assert_eq!(entry["action"], "update");
        assert_eq!(entry["resource"], "Note");
        assert_eq!(entry["provenance"], "provable");
    }

    /// AC-4 keystone: removing one `#[authorize]` from a handler removes
    /// exactly that binding's entry and leaves every other dimension
    /// byte-identical. This is the falsifiability claim the dimension makes —
    /// if the manifest kept reporting a binding whose attribute is gone, the
    /// "provable" tag would be a lie.
    #[test]
    fn removing_an_authorize_binding_flips_only_that_entry() {
        let dump = |bindings: &[(&str, &str)]| {
            vec![
                route("GET", "/health", "health", "framework"),
                route_with_bindings("POST", "/notes/{id}", "update_note", "gated", bindings),
            ]
        };
        let sec = security_dump(true, &[]);
        let both = manifest_value(&build_manifest(
            &dump(&[("delete", "Note"), ("update", "Note")]),
            Some(&sec),
        ));
        let one = manifest_value(&build_manifest(&dump(&[("update", "Note")]), Some(&sec)));

        // Every other dimension is untouched: the route is still `gated`, still
        // carries `policy: true`, and is still the same mutating CSRF entry.
        assert_eq!(both["dimensions"]["routes"], one["dimensions"]["routes"]);
        assert_eq!(both["dimensions"]["csrf"], one["dimensions"]["csrf"]);
        assert_eq!(
            both["dimensions"]["security_headers"],
            one["dimensions"]["security_headers"]
        );

        let entries = |v: &serde_json::Value| -> Vec<serde_json::Value> {
            v["dimensions"]["authorization_policies"]["entries"]
                .as_array()
                .expect("entries array")
                .clone()
        };
        let (both_entries, one_entries) = (entries(&both), entries(&one));
        assert_eq!(both_entries.len(), 2, "{both_entries:?}");
        assert_eq!(one_entries.len(), 1, "{one_entries:?}");

        // The difference is exactly the removed binding; the surviving entry is
        // byte-identical to the one it was listed beside.
        let removed: Vec<&serde_json::Value> = both_entries
            .iter()
            .filter(|e| !one_entries.contains(e))
            .collect();
        assert_eq!(removed.len(), 1, "exactly one entry may disappear");
        assert_eq!(removed[0]["action"], "delete");
        assert_eq!(removed[0]["resource"], "Note");
        assert_eq!(one_entries[0]["action"], "update");
    }

    /// The mirror claim (closing AC-7 in the other direction): changing a
    /// route's *classification* moves the `routes` dimension and nothing else.
    /// The route reclassified here is a safe-method GET with no bindings, so a
    /// leak into `csrf` or `authorization_policies` can only be a bug.
    #[test]
    fn reclassifying_a_route_flips_only_the_routes_dimension() {
        let dump = |cls: &str, roles: &[&str]| {
            let mut reports = route("GET", "/reports", "reports", cls);
            reports.roles = roles.iter().map(|r| (*r).to_owned()).collect();
            vec![
                reports,
                route_with_bindings(
                    "POST",
                    "/notes/{id}",
                    "update_note",
                    "gated",
                    &[("update", "Note")],
                ),
            ]
        };
        let sec = security_dump(true, &[]);
        let before = manifest_value(&build_manifest(&dump("unclassified", &[]), Some(&sec)));
        let after = manifest_value(&build_manifest(&dump("gated", &["admin"]), Some(&sec)));

        assert_ne!(
            before["dimensions"]["routes"], after["dimensions"]["routes"],
            "reclassifying a route must be visible in the routes dimension"
        );
        assert_eq!(before["dimensions"]["csrf"], after["dimensions"]["csrf"]);
        assert_eq!(
            before["dimensions"]["security_headers"],
            after["dimensions"]["security_headers"]
        );
        // Sanity: the comparison below is a real claim only while the dimension
        // actually has entries to differ in.
        assert!(
            !before["dimensions"]["authorization_policies"]["entries"]
                .as_array()
                .expect("entries array")
                .is_empty(),
            "the fixture must seed at least one binding"
        );
        assert_eq!(
            before["dimensions"]["authorization_policies"],
            after["dimensions"]["authorization_policies"]
        );
    }

    /// AC-6 ordering: entries are globally sorted by
    /// `(path, method, action, resource)` regardless of dump order, so the
    /// manifest diff stays reviewable.
    #[test]
    fn authorization_policies_entries_are_sorted() {
        let routes = vec![
            route_with_bindings(
                "POST",
                "/notes/{id}",
                "update_note",
                "gated",
                &[("update", "Note"), ("delete", "Note")],
            ),
            route_with_bindings(
                "GET",
                "/notes/{id}",
                "show_note",
                "gated",
                &[("show", "Note")],
            ),
            route_with_bindings(
                "POST",
                "/albums/{id}",
                "update_album",
                "gated",
                &[("update", "Photo"), ("update", "Album")],
            ),
        ];
        let manifest = build_manifest(&routes, None);
        let order: Vec<(&str, &str, &str, &str)> = manifest
            .dimensions
            .authorization_policies
            .entries
            .iter()
            .map(|e| {
                (
                    e.path.as_str(),
                    e.method.as_str(),
                    e.action.as_str(),
                    e.resource.as_str(),
                )
            })
            .collect();
        assert_eq!(
            order,
            vec![
                ("/albums/{id}", "POST", "update", "Album"),
                ("/albums/{id}", "POST", "update", "Photo"),
                ("/notes/{id}", "GET", "show", "Note"),
                ("/notes/{id}", "POST", "delete", "Note"),
                ("/notes/{id}", "POST", "update", "Note"),
            ],
            "entries sort by (path, method, action, resource)"
        );
    }

    /// D6 pin: `routes.entries[].policy` is a *superset* boolean.
    /// A `#[repository(policy = ...)]` auto-API route, or a handler calling
    /// `__check_policy` inline, dumps `policy: true` with no bindings — and
    /// must contribute no authorization entry rather than a fabricated one.
    /// That gap is disclosed by the `repository_policy_bindings` excluded entry.
    #[test]
    fn policy_true_without_bindings_produces_no_authorization_entry() {
        let mut r = route("POST", "/api/notes", "notes::create", "gated");
        r.policy = true;
        let json = manifest_value(&build_manifest(&[r], None));

        assert_eq!(json["dimensions"]["routes"]["entries"][0]["policy"], true);
        assert!(
            json["dimensions"]["authorization_policies"]["entries"]
                .as_array()
                .expect("entries array")
                .is_empty(),
            "a bare `policy: true` proves no (action, resource) binding"
        );

        let excluded = json["excluded"].as_array().expect("excluded array");
        let repo = excluded
            .iter()
            .find(|e| e["dimension"] == "repository_policy_bindings")
            .expect("repository_policy_bindings excluded");
        assert_eq!(repo["eventual_provenance"], "provable");
        assert!(
            repo["reason"]
                .as_str()
                .expect("reason string")
                .contains("routes.entries[].policy"),
            "the excluded entry must point readers at the superset boolean: {repo}"
        );
    }

    /// Defense in depth at the CLI layer: framework routes are mounted by the
    /// framework itself and can carry no `#[authorize]`, so
    /// `route_listing::authorize_bindings_of` already drops any binding on
    /// them. A dump claiming otherwise is malformed by construction, and the
    /// builder mirrors that rule instead of trusting its input.
    #[test]
    fn framework_routes_never_contribute_authorization_entries() {
        let mut framework = route_with_bindings(
            "POST",
            "/_autumn/unsubscribe",
            "unsubscribe",
            "framework",
            &[("update", "Subscriber")],
        );
        framework.source = "framework".to_owned();
        let routes = vec![
            framework,
            route_with_bindings(
                "POST",
                "/notes/{id}",
                "update_note",
                "gated",
                &[("update", "Note")],
            ),
        ];
        let manifest = build_manifest(&routes, None);
        let entries = &manifest.dimensions.authorization_policies.entries;
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].path, "/notes/{id}");
        assert!(
            !entries.iter().any(|e| e.path == "/_autumn/unsubscribe"),
            "no framework route may appear: {entries:?}"
        );
    }

    /// AC-2 falsifiability: one CSRF entry per mutating route; a safe-method
    /// (GET) route never appears in `csrf.entries`.
    #[test]
    fn csrf_dimension_only_covers_mutating_routes() {
        let routes = vec![
            route("GET", "/widgets", "list_widgets", "public"),
            route("POST", "/widgets", "create_widget", "gated"),
            route("DELETE", "/widgets/{id}", "delete_widget", "gated"),
        ];
        let sec = security_dump(true, &[]);
        let manifest = build_manifest(&routes, Some(&sec));
        let paths: Vec<(&str, &str)> = manifest
            .dimensions
            .csrf
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.method.as_str()))
            .collect();
        assert_eq!(
            paths,
            vec![("/widgets", "POST"), ("/widgets/{id}", "DELETE")],
            "only mutating routes, ordered by (path, method)"
        );
        assert!(
            manifest
                .dimensions
                .csrf
                .entries
                .iter()
                .all(|e| e.csrf_enforced)
        );
    }

    /// Codex P2 (issue #1627): a mutating framework route (the actuator's
    /// `PUT {prefix}/loggers/{name}`) that previously never reached the listing
    /// must, once enumerated, surface in `csrf.entries` with enforcement on.
    #[test]
    fn csrf_dimension_covers_mutating_framework_routes() {
        let routes = vec![
            route("GET", "/actuator/loggers", "loggers_get", "framework"),
            route(
                "PUT",
                "/actuator/loggers/{name}",
                "loggers_put",
                "framework",
            ),
        ];
        let dim = build_csrf_dimension(&routes, &security_dump(true, &[]).csrf);
        let entry = dim
            .entries
            .iter()
            .find(|e| e.method == "PUT" && e.path == "/actuator/loggers/{name}")
            .expect("mutating framework route must appear in csrf.entries");
        assert!(
            entry.csrf_enforced && !entry.exempt,
            "PUT loggers must be CSRF-enforced: {entry:?}"
        );
        // The safe-method GET listing route is never a csrf entry.
        assert!(
            !dim.entries.iter().any(|e| e.method == "GET"),
            "safe-method routes must not appear: {:?}",
            dim.entries
        );
    }

    /// AC-2 keystone: disabling CSRF flips exactly the affected `csrf.entries`
    /// and leaves `routes` and `security_headers` byte-identical.
    #[test]
    fn disabling_csrf_flips_only_csrf_entries() {
        let routes = vec![
            route("GET", "/health", "health", "framework"),
            route("POST", "/widgets", "create_widget", "gated"),
        ];
        let on = manifest_value(&build_manifest(&routes, Some(&security_dump(true, &[]))));
        let off = manifest_value(&build_manifest(&routes, Some(&security_dump(false, &[]))));

        // routes + security_headers untouched.
        assert_eq!(on["dimensions"]["routes"], off["dimensions"]["routes"]);
        assert_eq!(
            on["dimensions"]["security_headers"],
            off["dimensions"]["security_headers"]
        );
        // csrf.entries changed: enforced true → false for the mutating route.
        assert_eq!(
            on["dimensions"]["csrf"]["entries"][0]["csrf_enforced"],
            true
        );
        assert_eq!(
            off["dimensions"]["csrf"]["entries"][0]["csrf_enforced"],
            false
        );
    }

    /// AC-2 keystone: adding an exempt-path prefix flips exactly the entries
    /// under that prefix and records the exemption, leaving other dimensions
    /// byte-identical.
    #[test]
    fn adding_exempt_path_flips_only_affected_csrf_entries() {
        let routes = vec![
            route("POST", "/api/widgets", "api_create", "gated"),
            route("POST", "/widgets", "create_widget", "gated"),
        ];
        let base = manifest_value(&build_manifest(&routes, Some(&security_dump(true, &[]))));
        let exempt = manifest_value(&build_manifest(
            &routes,
            Some(&security_dump(true, &["/api/"])),
        ));

        assert_eq!(base["dimensions"]["routes"], exempt["dimensions"]["routes"]);
        assert_eq!(
            base["dimensions"]["security_headers"],
            exempt["dimensions"]["security_headers"]
        );

        // /api/widgets (sorts first) is now exempt & unenforced; /widgets is not.
        let e = &exempt["dimensions"]["csrf"]["entries"];
        assert_eq!(e[0]["path"], "/api/widgets");
        assert_eq!(e[0]["exempt"], true);
        assert_eq!(e[0]["csrf_enforced"], false);
        assert_eq!(e[1]["path"], "/widgets");
        assert_eq!(e[1]["exempt"], false);
        assert_eq!(e[1]["csrf_enforced"], true);
        // The exemption is recorded once at the dimension level.
        assert_eq!(exempt["dimensions"]["csrf"]["exempt_paths"][0], "/api/");
    }

    /// AC-3 keystone: weakening `x_frame_options` changes exactly that header
    /// entry; emptying the CSP flips exactly its `emitted`/`value` and nothing
    /// else. `routes` and `csrf` stay byte-identical.
    #[test]
    fn weakening_a_header_changes_only_that_entry() {
        let routes = vec![route("POST", "/widgets", "create_widget", "gated")];
        let base_sec = security_dump(true, &[]);
        let base = manifest_value(&build_manifest(&routes, Some(&base_sec)));

        // Weaken X-Frame-Options DENY → SAMEORIGIN.
        let mut weak = security_dump(true, &[]);
        weak.headers.x_frame_options = "SAMEORIGIN".to_owned();
        let weak = manifest_value(&build_manifest(&routes, Some(&weak)));

        assert_eq!(base["dimensions"]["routes"], weak["dimensions"]["routes"]);
        assert_eq!(base["dimensions"]["csrf"], weak["dimensions"]["csrf"]);

        let find = |v: &serde_json::Value, key: &str| -> serde_json::Value {
            v["dimensions"]["security_headers"]["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["header"] == key)
                .unwrap()
                .clone()
        };
        assert_eq!(find(&base, "x_frame_options")["value"], "DENY");
        assert_eq!(find(&weak, "x_frame_options")["value"], "SAMEORIGIN");
        // Every other header entry is unchanged.
        for key in ["content_security_policy", "referrer_policy"] {
            assert_eq!(
                find(&base, key),
                find(&weak, key),
                "{key} must be untouched"
            );
        }

        // Emptying the CSP flips emitted true → false and blanks the value.
        let mut no_csp = security_dump(true, &[]);
        no_csp.headers.content_security_policy = String::new();
        let no_csp = manifest_value(&build_manifest(&routes, Some(&no_csp)));
        assert_eq!(find(&base, "content_security_policy")["emitted"], true);
        assert_eq!(find(&no_csp, "content_security_policy")["emitted"], false);
        assert_eq!(find(&no_csp, "content_security_policy")["value"], "");
    }

    /// Finding C (default config): `#[ws]` routes are listed with the synthetic
    /// method `WS` but mounted as GET, and the default `safe_methods` includes
    /// GET, so runtime never validates a CSRF token on them. They must NOT appear
    /// in `csrf.entries` under the default configuration.
    #[test]
    fn ws_routes_are_csrf_safe_when_get_is_a_safe_method() {
        let routes = vec![
            route("WS", "/live", "live_socket", "public"),
            route("POST", "/widgets", "create_widget", "gated"),
        ];
        // Default `safe_methods` includes GET (see `security_dump`).
        let manifest = build_manifest(&routes, Some(&security_dump(true, &[])));
        let entries: Vec<(&str, &str)> = manifest
            .dimensions
            .csrf
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.method.as_str()))
            .collect();
        assert_eq!(
            entries,
            vec![("/widgets", "POST")],
            "WS routes mount as GET; with GET safe they must not be listed as mutating"
        );
        assert!(
            !manifest
                .dimensions
                .csrf
                .entries
                .iter()
                .any(|e| e.method == "WS"),
            "no WS entry may appear in the csrf dimension when GET is safe"
        );
    }

    /// Finding C (customized config): an app that removes GET from
    /// `security.csrf.safe_methods` makes runtime CSRF-validate the WS upgrade
    /// (a GET) unless its path is exempt. The manifest must reflect that: the WS
    /// route now appears in `csrf.entries`, its displayed `method` stays the
    /// route's listed `WS` (consistent with the `routes` dimension), and
    /// `csrf_enforced` is computed via the normal exempt-path predicate
    /// (enforced when the path isn't exempt, unenforced when it is).
    #[test]
    fn ws_routes_are_csrf_enforced_when_get_is_not_a_safe_method() {
        let routes = vec![
            route("WS", "/live", "live_socket", "public"),
            route("WS", "/api/live", "api_socket", "public"),
            route("POST", "/widgets", "create_widget", "gated"),
        ];
        // Customize config: drop GET, and exempt the `/api/` prefix.
        let mut sec = security_dump(true, &["/api/"]);
        sec.csrf.safe_methods = vec!["HEAD".to_owned(), "OPTIONS".to_owned()];
        let manifest = build_manifest(&routes, Some(&sec));

        let entries: Vec<(&str, &str, bool, bool)> = manifest
            .dimensions
            .csrf
            .entries
            .iter()
            .map(|e| {
                (
                    e.path.as_str(),
                    e.method.as_str(),
                    e.csrf_enforced,
                    e.exempt,
                )
            })
            .collect();
        // Ordered by (path, method): /api/live (WS), /live (WS), /widgets (POST).
        assert_eq!(
            entries,
            vec![
                ("/api/live", "WS", false, true),
                ("/live", "WS", true, false),
                ("/widgets", "POST", true, false),
            ],
            "WS routes appear as GET-mutating entries with the exempt-path predicate applied, \
             while keeping their listed `WS` method for display"
        );
    }

    /// Finding D: runtime's security-headers layer inserts a header only when
    /// `HeaderValue::from_str` succeeds. A non-empty but invalid value (e.g. one
    /// containing a newline) must be reported `emitted: false`, exposing the
    /// misconfiguration instead of overstating what responses send.
    #[test]
    fn invalid_header_value_is_not_emitted() {
        let routes = vec![route("POST", "/widgets", "create_widget", "gated")];
        let mut sec = security_dump(true, &[]);
        // A newline is rejected by `HeaderValue::from_str`.
        sec.headers.referrer_policy = "no-referrer\r\ninjected: 1".to_owned();
        let json = manifest_value(&build_manifest(&routes, Some(&sec)));
        let entry = json["dimensions"]["security_headers"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["header"] == "referrer_policy")
            .unwrap()
            .clone();
        assert_eq!(
            entry["emitted"], false,
            "an invalid (unparseable) header value must not be reported as emitted"
        );
    }

    /// Finding E: `csp_nonce` is a config toggle that shapes the CSP *value*, not
    /// a response header. Runtime stores the nonce in request extensions and
    /// substitutes it into `Content-Security-Policy`; it never emits a `csp_nonce`
    /// header. The `security_headers` dimension must therefore carry no
    /// `csp_nonce` entry, while the nonce toggle's effect stays visible through
    /// the nonce-aware `content_security_policy` value.
    #[test]
    fn csp_nonce_is_not_reported_as_an_emitted_header() {
        let routes = vec![route("GET", "/page", "page", "public")];
        // Mirror `SecurityDump::from_config` for a nonce-enabled app on the
        // default CSP: `csp_nonce = true` and `content_security_policy` resolved
        // to the nonce-aware template (see `resolved_content_security_policy`).
        let mut sec = security_dump(true, &[]);
        sec.headers.csp_nonce = true;
        sec.headers.content_security_policy =
            "style-src 'self' 'nonce-AUTUMN_CSP_NONCE'; script-src 'self' 'nonce-AUTUMN_CSP_NONCE'"
                .to_owned();
        let json = manifest_value(&build_manifest(&routes, Some(&sec)));

        let entries = json["dimensions"]["security_headers"]["entries"]
            .as_array()
            .unwrap();
        assert!(
            !entries.iter().any(|e| e["header"] == "csp_nonce"),
            "csp_nonce is not a response header and must not appear in security_headers"
        );

        // The nonce toggle's effect remains observable via the CSP entry value.
        let csp = entries
            .iter()
            .find(|e| e["header"] == "content_security_policy")
            .expect("content_security_policy entry present");
        assert_eq!(csp["emitted"], true);
        assert!(
            csp["value"]
                .as_str()
                .unwrap()
                .contains("'nonce-AUTUMN_CSP_NONCE'"),
            "the nonce-aware CSP template must remain visible in the CSP value: {csp}"
        );
    }

    /// Finding B (manifest side): a webhook endpoint path folded into the dump's
    /// `csrf.exempt_paths` (as `SecurityDump::from_config` does at runtime) makes
    /// a POST route on that path exempt and unenforced, and records the path in
    /// the dimension's `exempt_paths`.
    #[test]
    fn webhook_exempt_path_marks_route_unenforced() {
        let routes = vec![route("POST", "/webhooks/stripe", "stripe_hook", "public")];
        // The webhook path is NOT in `security.csrf.exempt_paths`; it arrives via
        // the webhook-endpoint fold performed by `SecurityDump::from_config`.
        let sec = security_dump(true, &["/webhooks/stripe"]);
        let manifest = build_manifest(&routes, Some(&sec));
        let entry = &manifest.dimensions.csrf.entries[0];
        assert_eq!(entry.path, "/webhooks/stripe");
        assert!(entry.exempt, "webhook path must be exempt");
        assert!(
            !entry.csrf_enforced,
            "runtime skips CSRF on webhook endpoints, so csrf_enforced must be false"
        );
        assert!(
            manifest
                .dimensions
                .csrf
                .exempt_paths
                .iter()
                .any(|p| p == "/webhooks/stripe"),
            "dimension exempt_paths must record the webhook path"
        );
    }

    /// The framework-owned RFC 8058 one-click unsubscribe endpoint is mounted as
    /// a GET + POST at `/_autumn/unsubscribe` and its path is folded into the
    /// dump's `csrf.exempt_paths` (by `SecurityDump::from_config`). Once the POST
    /// route is listed by `append_framework_routes`, the csrf dimension must show
    /// it as present-but-exempt: `csrf_enforced == false && exempt == true`,
    /// matching runtime (which exempts the path from CSRF). Underreporting it
    /// would hide a mutating route from the posture manifest.
    #[test]
    fn unsubscribe_exempt_path_marks_post_unenforced() {
        // Path literal mirrors `autumn_web::mail::UNSUBSCRIBE_PATH`; kept literal
        // so this dimension-level test does not require the `mail` feature.
        let routes = vec![route(
            "POST",
            "/_autumn/unsubscribe",
            "unsubscribe",
            "framework",
        )];
        let sec = security_dump(true, &["/_autumn/unsubscribe"]);
        let manifest = build_manifest(&routes, Some(&sec));
        let entry = &manifest.dimensions.csrf.entries[0];
        assert_eq!(entry.path, "/_autumn/unsubscribe");
        assert_eq!(entry.method, "POST");
        assert!(entry.exempt, "mounted unsubscribe path must be exempt");
        assert!(
            !entry.csrf_enforced,
            "runtime exempts the unsubscribe endpoint from CSRF, so csrf_enforced must be false"
        );
        assert!(
            manifest
                .dimensions
                .csrf
                .exempt_paths
                .iter()
                .any(|p| p == "/_autumn/unsubscribe"),
            "dimension exempt_paths must record the unsubscribe path"
        );
    }

    /// AC-6: two builds of identical source + config produce byte-identical
    /// manifest JSON. Seeded with a multi-binding route so the
    /// `authorization_policies` dimension is exercised too.
    #[test]
    fn manifest_rebuild_is_byte_identical() {
        let routes = vec![
            route("GET", "/health", "health", "framework"),
            route("POST", "/widgets", "create_widget", "gated"),
            route_with_bindings(
                "DELETE",
                "/widgets/{id}",
                "delete_widget",
                "gated",
                &[("delete", "Widget"), ("update", "Widget")],
            ),
        ];
        let sec = security_dump(true, &["/api/", "/webhooks/"]);
        let a = manifest_json(&build_manifest(&routes, Some(&sec)));
        let b = manifest_json(&build_manifest(&routes, Some(&sec)));
        assert_eq!(a, b, "manifest must be reproducible byte-for-byte");
        assert!(
            a.contains("\"resource\": \"Widget\""),
            "the seeded bindings must appear in the manifest: {a}"
        );
    }

    /// The `parse_security_config` reader recovers the dump from a stderr stream
    /// containing the marker line amongst other output.
    #[test]
    fn parse_security_config_reads_the_marker() {
        let sec = security_dump(true, &["/api/"]);
        let line = serde_json::to_string(&sec).unwrap();
        let stderr = format!(
            "\u{1F342} autumn routes audit\n{SECURITY_CONFIG_MARKER}{line}\nsome trailing warning\n"
        );
        let parsed = parse_security_config(&stderr).expect("marker parsed");
        assert!(parsed.csrf.enabled);
        assert_eq!(parsed.csrf.exempt_paths, vec!["/api/"]);
        assert!(parse_security_config("no marker here\n").is_none());
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
    /// declarations — as the first-party `AdminPlugin` does via the documented
    /// `nest(prefix, router).declare_plugin_routes(routes)` pattern — is
    /// enumerable, so the app emits NO omitted marker (`hidden == 0`) because a
    /// declared route falls under the nest prefix. `parse_omitted_count` returns
    /// 0 and, with clean visible routes, the combined gate must PASS. Previously
    /// the nested admin router still tripped the marker and false-failed the
    /// audit.
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
