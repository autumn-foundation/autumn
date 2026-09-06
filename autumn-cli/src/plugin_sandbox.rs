//! `autumn plugin package` / `autumn plugin inspect` — the consent surface.
//!
//! A sandboxed plugin arrives as opaque bytes. These two commands are what turn
//! that into a decision an operator can actually make:
//!
//! ```text
//! autumn plugin package --manifest plugin.toml --module target/…/plugin.wasm \
//!                       --out hello.autumn-plugin
//! autumn plugin inspect hello.autumn-plugin
//! ```
//!
//! `package` binds a manifest to a module and stamps the module's digest, so an
//! author cannot ship a manifest that describes different bytes than the ones
//! in the box. `inspect` is the consent screen: what the plugin may do, what it
//! may not, which bytes were reviewed, every host function the module imports —
//! and a real verdict, because it loads the module into **the same sandbox the
//! runtime uses** and runs the same conformance checks `autumn plugin-check`
//! runs against a native plugin.
//!
//! Both refuse rather than warn. A plugin that would fail at the operator's
//! boot should fail at the author's desk.

use std::path::Path;

use autumn_web::plugin_sandbox::{
    ConsentDelta, MAX_MANIFEST_BYTES, MAX_MODULE_BYTES, SandboxArtifact, SandboxHost,
    SandboxManifest, read_bounded,
};
use serde::Serialize;

use crate::plugin_check::{self, ConformanceReport, ReportFormat};
use crate::routes::RouteInfo;

/// Inputs to `autumn plugin package`.
pub struct PackageOptions<'a> {
    /// The authored manifest, as TOML.
    pub manifest: &'a Path,
    /// The `wasm32-wasip1` module the manifest describes.
    pub module: &'a Path,
    /// Where to write the `.autumn-plugin` artifact.
    pub out: &'a Path,
}

/// Bind a manifest to a module and write the artifact.
///
/// The module's digest is computed here and stamped into the manifest, so the
/// author never types it and can never get it wrong. The module is also loaded
/// into the sandbox before anything is written: an artifact that could not run
/// is not an artifact worth shipping.
///
/// # Errors
///
/// Returns a human-readable message when either input cannot be read, the
/// manifest does not validate, the payload is not a WebAssembly module, or the
/// module imports something the sandbox does not provide.
pub fn package(opts: &PackageOptions<'_>) -> Result<SandboxArtifact, String> {
    // Bounded reads, for the same reason the loader's are: a ceiling applied
    // after `fs::read` returns is a decision made after the damage. Packaging
    // takes both of its inputs from whoever ran the command, and a
    // multi-gigabyte one should be an error message rather than an OOM.
    let manifest_bytes = read_bounded(opts.manifest, MAX_MANIFEST_BYTES)
        .map_err(|err| format!("{}: {err}", opts.manifest.display()))?;
    let manifest_src = String::from_utf8(manifest_bytes)
        .map_err(|_| format!("{} is not valid UTF-8", opts.manifest.display()))?;
    let manifest = SandboxManifest::parse(&manifest_src).map_err(|err| err.to_string())?;
    let module = read_bounded(opts.module, MAX_MODULE_BYTES)
        .map_err(|err| format!("{}: {err}", opts.module.display()))?;

    let artifact = SandboxArtifact::seal(manifest, module).map_err(|err| err.to_string())?;
    // Load it before writing it. Refusing here costs the author a second;
    // refusing at the operator's boot costs them a deploy.
    SandboxHost::load(&artifact).map_err(|err| err.to_string())?;

    artifact
        .write_file(opts.out)
        .map_err(|err| err.to_string())?;
    Ok(artifact)
}

/// Read and verify an artifact from disk.
///
/// # Errors
///
/// Returns a human-readable message when the file cannot be read, is not an
/// Autumn plugin artifact, or does not match its own declared digest.
pub fn inspect(path: &Path) -> Result<SandboxArtifact, String> {
    SandboxArtifact::read_file(path).map_err(|err| err.to_string())
}

/// What each granted capability is scoped to, as the JSON report renders it.
#[derive(Debug, Default, Serialize)]
pub struct ReportGrants {
    /// Hostnames `http-outbound` may call.
    pub hosts: Vec<String>,
    /// Logical tables `db` owns.
    pub tables: Vec<String>,
    /// Job types `jobs` may enqueue.
    pub job_types: Vec<String>,
    /// Render slots `render` may fill.
    pub slots: Vec<String>,
}

/// One declared route, as the JSON report renders it.
#[derive(Debug, Serialize)]
pub struct ReportRoute {
    /// HTTP method.
    pub method: String,
    /// Full mounted path.
    pub path: String,
}

/// The operator-facing verdict on an artifact.
#[derive(Debug, Serialize)]
pub struct Report {
    /// Plugin name.
    pub name: String,
    /// Plugin version.
    pub version: String,
    /// The prefix it mounts under.
    pub prefix: String,
    /// The module digest the manifest declares and the loader verifies.
    ///
    /// Not the review identity: it answers "are these the author's bytes", and
    /// stays correct when the manifest around them is rewritten.
    pub sha256: String,
    /// The digest of the whole artifact — manifest and module together.
    ///
    /// This is the number to record and compare, because a review is of the
    /// grant as much as of the code. `None` if the container could not be
    /// re-rendered to digest it, which is reported rather than papered over.
    pub artifact_sha256: Option<String>,
    /// Capabilities the manifest asks for.
    pub capabilities: Vec<String>,
    /// What each granted capability is scoped to (issue #1632).
    ///
    /// A capability name answers "may it?" and nothing in it answers "to
    /// what?", so an automated policy check reading only `capabilities` would
    /// approve `http-outbound` without ever seeing which hosts. These are the
    /// manifest's `[grants]` lists, verbatim.
    pub grants: ReportGrants,
    /// The per-request capability quotas the manifest declares.
    pub quotas: std::collections::BTreeMap<String, u32>,
    /// Classes of authority this build denies unconditionally.
    pub denied: Vec<String>,
    /// The routes it serves.
    pub routes: Vec<ReportRoute>,
    /// Every host function the module imports, or `None` when they could not
    /// be read at all.
    ///
    /// An empty list and an unreadable one are different facts, and collapsing
    /// them printed `(none)` — "this artifact asks for no host authority" — for
    /// a module whose shape was refused before its imports could be enumerated.
    /// That is the consent screen stating the opposite of the truth about
    /// exactly the artifacts whose authority matters most.
    pub imports: Option<Vec<String>>,
    /// Whether the module loads into this build's sandbox.
    pub loads: bool,
    /// Why it did not load, when it did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_error: Option<String>,
    /// The route-conformance report.
    pub conformance: ConformanceReport,
    /// The full consent summary, verbatim.
    pub consent: String,
    /// What this manifest asks for that the one named by `--against` did not
    /// (issue #1632). `None` when no previous artifact was named.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<ConsentDelta>,
}

/// Authority the sandbox denies unconditionally in this version.
///
/// Rendered alongside the grant because a consent screen that only lists what
/// was asked for tells a reader nothing about the shape of the "no".
const DENIED_CLASSES: &[&str] = &[
    "filesystem",
    "network",
    "environment",
    "database",
    "process-control",
];

/// Whether this manifest was granted the authority `class` names.
///
/// Maps the coarse class names an operator reads on the consent screen onto the
/// capability vocabulary, so the "denied" list shrinks as a manifest asks for
/// more rather than contradicting it.
fn granted_class(manifest: &SandboxManifest, class: &str) -> bool {
    use autumn_web::plugin_sandbox::SandboxCapability;
    match class {
        "network" => manifest.is_granted(SandboxCapability::HttpOutbound),
        "database" => {
            manifest.is_granted(SandboxCapability::Db) || manifest.is_granted(SandboxCapability::Kv)
        }
        // Filesystem, environment and process control have no capability that
        // grants them and are not on the roadmap to; they stay in the list
        // whatever the manifest asks for.
        _ => false,
    }
}

/// Characters of artifact-supplied text this screen will render.
const EXCERPT: usize = 512;

/// Render text lifted out of an unaudited artifact, bounded and neutralised.
///
/// Both halves matter and neither substitutes for the other. The **escaping**
/// is why this screen can be trusted at all: a terminal escape in an import
/// name could rewrite the lines above it — the routes, the grant, the verdict —
/// which is an attack on the decision the screen exists to inform.
///
/// The **bound** is why rendering it is safe to attempt. `escape_debug()` on a
/// name that fills most of the 64 MiB module allowance expands every byte
/// before anything truncates the result, so `inspect` could exhaust memory on
/// the very artifact it is meant to refuse. Truncating during the expansion
/// rather than after it is the whole point: the long string is never built.
fn excerpt(text: &str) -> String {
    let mut out = String::new();
    for (kept, ch) in text.chars().enumerate() {
        if kept == EXCERPT {
            out.push_str(" … (truncated)");
            break;
        }
        out.extend(ch.escape_debug());
    }
    out
}

impl Report {
    /// Build the verdict for a verified artifact.
    #[must_use]
    pub fn of(artifact: &SandboxArtifact) -> Self {
        let manifest = artifact.manifest();
        // The import list is a property of the module, not of whether the
        // sandbox will run it — and it is *most* interesting when the sandbox
        // will not. Reporting "(none)" for a module that was refused for what
        // it imports would make the consent screen contradict itself.
        // Excerpted *here*, not where they are rendered. Both strings come out
        // of an artifact nobody has audited, and `to_json` serializes the
        // report's own fields — so a bound applied in `to_text` leaves the JSON
        // path carrying a name that fills most of the module allowance, which
        // `serde_json` then expands again escaping it. Capping at construction
        // is what makes every renderer safe rather than the one that remembered.
        let imports = SandboxHost::imports_of(artifact.module())
            .ok()
            .map(|imports| imports.iter().map(|import| excerpt(import)).collect());
        let (loads, load_error) = match SandboxHost::load(artifact) {
            Ok(_) => (true, None),
            Err(err) => (false, Some(excerpt(&err.to_string()))),
        };
        Self {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            prefix: manifest.prefix.clone(),
            sha256: manifest.sha256.clone(),
            artifact_sha256: artifact.artifact_digest().ok(),
            capabilities: manifest
                .capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
            grants: ReportGrants {
                hosts: manifest.grants.hosts.clone(),
                tables: manifest.grants.tables.clone(),
                job_types: manifest.grants.job_types.clone(),
                slots: manifest.grants.slots.clone(),
            },
            quotas: manifest
                .quotas
                .fields()
                .into_iter()
                .map(|(field, value)| (field.to_owned(), value))
                .collect(),
            // Only the classes this build cannot grant *at all*, minus anything
            // this manifest was actually granted. A screen that printed "no
            // database access" under a manifest holding `db` would be a consent
            // screen contradicting itself, and a reader believes the reassuring
            // half.
            denied: DENIED_CLASSES
                .iter()
                .filter(|class| !granted_class(manifest, class))
                .map(|&name| name.to_owned())
                .collect(),
            // From `route_infos`, not from `manifest.routes`: HTTP serves HEAD
            // wherever it serves GET, and the runtime mounts it, so a manifest
            // declaring only GET serves a method its own literal route list
            // never names. The human surfaces already say so — `consent_summary`
            // prints the implied HEAD and `autumn routes` reports it — and this
            // is the surface a policy check reads. Underreporting here is how an
            // automated approval signs off on a route set smaller than the one
            // the artifact actually answers on.
            routes: manifest
                .route_infos()
                .into_iter()
                .map(|route| ReportRoute {
                    method: route.method,
                    path: route.path,
                })
                .collect(),
            imports,
            loads,
            load_error,
            conformance: conformance(manifest),
            consent: manifest.consent_summary(),
            upgrade: None,
        }
    }

    /// Whether the artifact is fit to install.
    ///
    /// The artifact digest is part of the verdict, not decoration beside it.
    /// `SandboxedPlugin::from_artifact` computes the same digest and refuses an
    /// artifact whose container cannot be re-rendered — a manifest that fits
    /// under the size limit on disk can exceed it once written back in
    /// canonical form — so a report that passed without one said "fit to
    /// install" about an artifact the runtime would reject. The digest is also
    /// the number the consent screen asks an operator to record, and there is
    /// nothing to record when it is missing.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.loads
            && self.artifact_sha256.is_some()
            && self.conformance.passed()
            // An upgrade that asks for more authority is not a failed artifact
            // — it may be exactly what the operator wants — but it is not one
            // an unattended `inspect` may wave through. Failing here is what
            // makes `--against` a gate rather than a note: a pipeline that
            // installs on a passing verdict stops and asks.
            && self
                .upgrade
                .as_ref()
                .is_none_or(|delta| !delta.requires_consent())
    }

    /// Render the report for a human.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = self.consent.clone();
        // Printed right under the consent block, because it is the number this
        // screen is asking to be trusted on. The module digest above it answers
        // a narrower question — "are these the author's bytes" — and stays
        // correct if the grant beside them is rewritten, so an operator
        // comparing that one could match a wider artifact than they approved.
        match self.artifact_sha256.as_deref() {
            Some(digest) => {
                out.push_str("  artifact sha256 (record this one): ");
                out.push_str(digest);
                out.push('\n');
            }
            None => out.push_str(
                "  artifact sha256: (could not be computed; review the manifest by hand)\n",
            ),
        }
        // Above the imports and right under the digest, because it is the one
        // block on this screen that answers "has anything changed since I
        // agreed" — an operator re-reading a familiar screen needs it before
        // their eyes glaze.
        if let Some(delta) = self.upgrade.as_ref() {
            if delta.requires_consent() {
                out.push_str("  \u{26A0} ");
                out.push_str(&delta.summary().replace('\n', "\n  "));
                out.push('\n');
            } else {
                out.push_str(
                    "  this upgrade asks for no authority the installed version did not\n",
                );
            }
        }
        out.push_str("  host functions it imports:\n");
        match self.imports.as_ref() {
            // Not the same as none, and saying so matters: the artifacts whose
            // imports cannot be read are the ones already refused for their
            // shape, which is when an operator most needs the screen not to
            // claim they ask for nothing.
            None => out
                .push_str("    (could not be read; the module was refused before they could be)\n"),
            Some(imports) if imports.is_empty() => out.push_str("    (none)\n"),
            Some(imports) => {
                for import in imports {
                    // Import names are arbitrary UTF-8 lifted straight out of a
                    // module an operator has explicitly not audited. A terminal
                    // escape here could rewrite the lines above it — the routes,
                    // the grant, the verdict — which is an attack on the
                    // decision this screen exists to inform.
                    out.push_str("    ");
                    // Already excerpted in `of`; escaping twice would render
                    // `\n` as `\\n`.
                    out.push_str(import);
                    out.push('\n');
                }
            }
        }
        out.push('\n');
        match &self.load_error {
            Some(error) => {
                out.push_str("\u{2717} this artifact does not load in this build's sandbox:\n  ");
                out.push_str(error);
                out.push('\n');
            }
            None => out.push_str("\u{2713} loads into this build's sandbox\n"),
        }
        out.push('\n');
        out.push_str(&self.conformance.to_text_report());
        out
    }

    /// Render the report as JSON.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message if the report cannot be serialized.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|err| err.to_string())
    }
}

/// Run the existing route-conformance checks over a manifest's declared routes.
///
/// A sandboxed plugin needs no binary built and no process run: the manifest
/// *is* the route table, and the runtime mounts exactly it. So the same checks
/// `autumn plugin-check` applies to a native plugin apply here, offline.
fn conformance(manifest: &SandboxManifest) -> ConformanceReport {
    let routes: Vec<RouteInfo> = manifest
        .routes
        .iter()
        .map(|route| RouteInfo {
            method: route.method.clone(),
            path: route.path.clone(),
            handler: format!("sandbox:{}", manifest.name),
            source: format!("plugin:{}", manifest.name),
            middleware: vec!["sandboxed".to_owned()],
            api_version: None,
            status: None,
            sunset_opt_out: None,
            // Inert at this site, and deliberately left so. The capacity
            // contract (#1733) derives these from a handler's declared
            // extractors at macro-expansion time; a sandboxed route has no
            // such handler, `build_report` reads neither field, and neither is
            // serialized — `ConformanceReport` carries only the checks. Giving
            // them a computed-looking value here would publish a claim nothing
            // derived and nothing reads. Empty matches every other
            // `RouteInfo` built by hand in this crate.
            pools: Vec::new(),
            resource_shape: String::new(),
        })
        .collect();

    // A sandboxed plugin holds no session or auth capability, so a route it
    // serves is unauthenticated by construction. Declaring that here is the
    // honest answer to the sensitive-surface check — the gating is the sandbox
    // itself.
    //
    // The grant is named in full rather than summarised as "no database or
    // network": since #1632 a manifest may hold `db`, `kv`, `http-outbound`,
    // `jobs` or `render`, and a check line that says otherwise would be wrong
    // about exactly the artifacts whose authority matters. `capabilities`
    // answers "may it?"; the scope lists answer "to what?", and an automated
    // reader needs both.
    let mut granted: Vec<String> = manifest
        .capabilities
        .iter()
        .map(|capability| capability.as_str().to_owned())
        .collect();
    for (label, entries) in [
        ("hosts", &manifest.grants.hosts),
        ("tables", &manifest.grants.tables),
        ("job_types", &manifest.grants.job_types),
        ("slots", &manifest.grants.slots),
    ] {
        if !entries.is_empty() {
            granted.push(format!("{label}={}", entries.join("|")));
        }
    }
    let granted = granted.join(", ");
    let gating =
        format!("sandboxed: no session, auth, filesystem or environment capability ({granted})");
    let sensitive = vec![plugin_check::SensitiveRouteDecl {
        path_pattern: manifest.prefix.clone(),
        auth_mechanism: gating.clone(),
    }];

    let mut report = plugin_check::build_report(
        &plugin_check::PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: &manifest.name,
            expected_prefix: Some(&manifest.prefix),
            sensitive_routes: &sensitive,
            format: ReportFormat::Text,
            // `Absent`, and deliberately so. A `ContractDump` reports what a
            // built binary wrote to its stderr (#1601) — and this path never
            // builds or runs one: a sandboxed plugin is a `.wasm` artifact, and
            // the report is derived from its manifest alone. `Present(vec![])`
            // would be the stronger claim that the plugin declared no
            // contracts, which is not something anything here established.
            // Both contract checks resolve to `Skip` on `Absent`, which is the
            // honest verdict for a check that could not be evaluated.
            contracts: &plugin_check::ContractDump::Absent,
            // The sandbox lane exposes no `--deny-experimental`, and the flag
            // fails closed on an unread contract — passing `true` would hard
            // fail every sandboxed artifact over a contract that cannot exist
            // for one. `false` leaves the check a `Skip`.
            deny_experimental: false,
        },
        &routes,
    );

    // The gating string above reaches the report only through the
    // sensitive-surfaces check, and that check only looks at routes whose path
    // is *named* like a sensitive one (`/admin`, `/debug`, …). A plugin that
    // holds `db` and `http-outbound` under the prefix `/shop` is exactly as
    // authoritative and would have been reported with the grant nowhere in
    // sight. So the grant gets a check of its own, present on every sandboxed
    // artifact: an automated reader gating an install on this report needs to
    // see the authority whether or not the path happens to say "admin".
    //
    // `Pass`, because a declared grant is conformant — `SandboxManifest::parse`
    // has already refused an ungrantable host, table, job type or slot by the
    // time this runs. The check reports authority; it does not adjudicate it.
    // Consent is `autumn plugin inspect`'s job, and it fails the verdict there.
    report.checks.push(plugin_check::CheckResult {
        name: "capability-grants".to_owned(),
        status: plugin_check::CheckStatus::Pass,
        message: gating,
        diagnostics: Vec::new(),
    });
    report
}

/// Run `autumn plugin package`, printing the consent summary and exiting
/// non-zero on refusal.
pub fn run_package(opts: &PackageOptions<'_>) {
    eprintln!("\u{1F342} autumn plugin package\n");
    match package(opts) {
        Ok(artifact) => {
            print!("{}", artifact.manifest().consent_summary());
            println!("\nWrote {}", opts.out.display());
        }
        Err(err) => {
            eprintln!("\u{2717} {err}");
            std::process::exit(1);
        }
    }
}

/// Run `autumn plugin inspect`, printing the consent screen and exiting
/// non-zero when the artifact is not fit to install.
pub fn run_inspect(path: &Path, format: &ReportFormat, against: Option<&Path>) {
    if matches!(format, ReportFormat::Text) {
        eprintln!("\u{1F342} autumn plugin inspect\n");
    }
    let artifact = inspect(path).unwrap_or_else(|err| {
        eprintln!("\u{2717} {err}");
        std::process::exit(1);
    });
    let mut report = Report::of(&artifact);
    if let Some(previous) = against {
        let previous = inspect(previous).unwrap_or_else(|err| {
            eprintln!("\u{2717} {err}");
            std::process::exit(1);
        });
        // A delta against a *different* plugin is not an upgrade, and reading
        // it as one is worse than not reading it at all: the candidate would
        // inherit the unrelated baseline's capabilities, grants, routes and
        // limits and come back "asks for no authority", which is exactly the
        // verdict an unattended install is gated on. Refused rather than
        // compared, because there is no answer to give.
        let (name, baseline) = (&artifact.manifest().name, &previous.manifest().name);
        if name != baseline {
            eprintln!(
                "\u{2717} --against names `{baseline}`, but this artifact is `{name}`; an upgrade \
                 is compared against an earlier build of the same plugin"
            );
            std::process::exit(1);
        }
        report.upgrade = Some(artifact.manifest().consent_delta_from(previous.manifest()));
    }
    match format {
        ReportFormat::Text => print!("{}", report.to_text()),
        ReportFormat::Json => match report.to_json() {
            Ok(json) => println!("{json}"),
            Err(err) => {
                eprintln!("\u{2717} {err}");
                std::process::exit(1);
            }
        },
    }
    if !report.passed() {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const MODULE: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start") (nop))
)
"#;

    #[test]
    fn artifact_text_is_bounded_and_escaped_before_it_is_rendered() {
        // Both halves, on one string. The name is long enough that expanding it
        // whole would dwarf the excerpt, and it carries the escape sequence a
        // hostile artifact would use to rewrite the verdict printed above it.
        let hostile = format!("wasi_snapshot_preview1::\u{1b}[2K{}", "a".repeat(100_000));
        let rendered = excerpt(&hostile);

        assert!(
            rendered.len() < 4 * 1024,
            "the whole name was expanded: {} bytes",
            rendered.len()
        );
        assert!(rendered.contains("truncated"), "{rendered}");
        assert!(
            !rendered.contains('\u{1b}'),
            "an escape survived and can repaint the operator's screen"
        );
        // Still legible enough to act on — an operator has to recognise which
        // import the refusal is about.
        assert!(
            rendered.starts_with("wasi_snapshot_preview1::"),
            "{rendered}"
        );
    }

    fn manifest_toml() -> String {
        // The digest is stamped by packaging, so an authored manifest carries a
        // placeholder — proving `package` computes it rather than trusting it.
        format!(
            r#"
name = "autumn-plugin-hello"
version = "0.1.0"
wire_version = 1
prefix = "/hello"
capabilities = ["http-request"]
sha256 = "{digest}"

[[routes]]
method = "GET"
path = "/hello/greet"
"#,
            digest = "0".repeat(64)
        )
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        manifest: PathBuf,
        module: PathBuf,
        out: PathBuf,
    }

    fn fixture(manifest_src: &str, module_bytes: Vec<u8>) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = dir.path().join("plugin.toml");
        let module = dir.path().join("plugin.wasm");
        let out = dir.path().join("plugin.autumn-plugin");
        std::fs::write(&manifest, manifest_src).expect("writes");
        std::fs::write(&module, module_bytes).expect("writes");
        Fixture {
            _dir: dir,
            manifest,
            module,
            out,
        }
    }

    fn good_fixture() -> Fixture {
        fixture(&manifest_toml(), wat::parse_str(MODULE).expect("valid WAT"))
    }

    fn pack(fixture: &Fixture) -> SandboxArtifact {
        package(&PackageOptions {
            manifest: &fixture.manifest,
            module: &fixture.module,
            out: &fixture.out,
        })
        .expect("packages")
    }

    // ── The grown vocabulary (issue #1632) ───────────────────────────

    /// The same plugin, asking for the whole vocabulary.
    fn granted_manifest_toml() -> String {
        format!(
            r#"
name = "autumn-plugin-hello"
version = "0.1.0"
wire_version = 1
prefix = "/hello"
capabilities = ["http-request", "kv", "http-outbound", "db", "jobs"]
sha256 = "{digest}"

[[routes]]
method = "GET"
path = "/hello/greet"

[grants]
hosts = ["api.example.com"]
tables = ["orders"]
job_types = ["reindex"]
"#,
            digest = "0".repeat(64)
        )
    }

    fn granted_fixture() -> Fixture {
        fixture(
            &granted_manifest_toml(),
            wat::parse_str(MODULE).expect("valid WAT"),
        )
    }

    #[test]
    fn the_report_names_what_each_capability_is_scoped_to() {
        let fixture = granted_fixture();
        let report = Report::of(&pack(&fixture));
        assert_eq!(report.grants.hosts, vec!["api.example.com".to_owned()]);
        assert_eq!(report.grants.tables, vec!["orders".to_owned()]);
        assert_eq!(report.grants.job_types, vec!["reindex".to_owned()]);
        assert!(report.quotas.contains_key("kv_reads"));

        let text = report.to_text();
        for expected in ["api.example.com", "orders", "reindex", "kv_reads"] {
            assert!(text.contains(expected), "{expected} missing from:\n{text}");
        }
        let value: serde_json::Value =
            serde_json::from_str(&report.to_json().expect("json")).expect("parses");
        assert_eq!(value["grants"]["hosts"][0], "api.example.com");
        assert!(value["quotas"]["outbound_calls"].is_number());
    }

    #[test]
    fn the_consent_screen_stops_denying_what_the_manifest_was_just_granted() {
        // A screen that prints "no database access" over a manifest holding
        // `db` is a screen contradicting itself, and the reader believes the
        // reassuring half.
        let granted = Report::of(&pack(&granted_fixture()));
        assert!(!granted.denied.contains(&"database".to_owned()));
        assert!(!granted.denied.contains(&"network".to_owned()));
        assert!(granted.denied.contains(&"filesystem".to_owned()));

        let bare = Report::of(&pack(&good_fixture()));
        for class in ["database", "network", "filesystem", "environment"] {
            assert!(bare.denied.contains(&class.to_owned()), "{class}");
        }
    }

    #[test]
    fn the_plugin_check_gating_line_carries_the_grant_and_its_scope() {
        // `autumn plugin-check` reads this line, and since #1632 a summary that
        // said "no database or network" would be wrong about exactly the
        // artifacts whose authority matters most.
        let report = conformance(pack(&granted_fixture()).manifest());
        let text = report.to_text_report();
        for expected in ["http-outbound", "hosts=api.example.com", "tables=orders"] {
            assert!(text.contains(expected), "{expected} missing from:\n{text}");
        }
        assert!(!text.contains("or database capability"), "{text}");
    }

    #[test]
    fn an_upgrade_baseline_naming_another_plugin_is_refused_rather_than_compared() {
        // `--against` pointed at the wrong installed artifact is a typo an
        // operator makes once. Comparing anyway would let a new plugin inherit
        // the unrelated baseline's capabilities, grants, routes and limits and
        // come back "asks for no authority" — the exact verdict an unattended
        // install is gated on.
        let next = pack(&granted_fixture());
        let other = pack(&fixture(
            &granted_manifest_toml().replace(
                &format!("name = \"{}\"", next.manifest().name),
                "name = \"autumn-plugin-other\"",
            ),
            wat::parse_str(MODULE).expect("valid WAT"),
        ));
        assert_ne!(next.manifest().name, other.manifest().name);

        let delta = next.manifest().consent_delta_from(other.manifest());
        assert!(
            !delta.requires_consent(),
            "the delta itself sees no new authority, which is exactly why the *names* have to \
             be checked before one is taken: {delta:?}"
        );
    }

    #[test]
    fn an_upgrade_that_asks_for_more_fails_the_verdict_until_a_human_agrees() {
        let previous = pack(&good_fixture());
        let next = pack(&granted_fixture());

        let mut report = Report::of(&next);
        assert!(report.passed(), "the artifact itself is fit to install");
        report.upgrade = Some(next.manifest().consent_delta_from(previous.manifest()));
        assert!(
            !report.passed(),
            "an upgrade that grows authority must stop an unattended install"
        );
        let text = report.to_text();
        assert!(text.contains("api.example.com"), "{text}");
        assert!(text.contains("http-outbound"), "{text}");

        // The same artifact against itself asks for nothing new.
        let mut same = Report::of(&next);
        same.upgrade = Some(next.manifest().consent_delta_from(next.manifest()));
        assert!(same.passed());
        assert!(
            same.to_text().contains("asks for no authority"),
            "{}",
            same.to_text()
        );

        // Asking for *less* is not a prompt: re-prompting for a narrowed grant
        // trains operators to click through the prompt that matters.
        let mut narrowed = Report::of(&previous);
        narrowed.upgrade = Some(previous.manifest().consent_delta_from(next.manifest()));
        assert!(narrowed.passed());
    }

    #[test]
    fn packaging_stamps_the_digest_the_author_could_not_know() {
        let fixture = good_fixture();
        let artifact = pack(&fixture);
        let module = std::fs::read(&fixture.module).expect("reads");
        assert_eq!(artifact.manifest().sha256, SandboxArtifact::digest(&module));
        assert_ne!(artifact.manifest().sha256, "0".repeat(64));
    }

    #[test]
    fn a_packaged_artifact_reads_back() {
        let fixture = good_fixture();
        pack(&fixture);
        let bytes = std::fs::read(&fixture.out).expect("reads");
        let artifact = SandboxArtifact::read(&bytes).expect("reads back");
        assert_eq!(artifact.manifest().name, "autumn-plugin-hello");
    }

    #[test]
    fn packaging_refuses_a_payload_that_is_not_wasm() {
        let fixture = fixture(&manifest_toml(), b"#!/bin/sh\n".to_vec());
        let err = package(&PackageOptions {
            manifest: &fixture.manifest,
            module: &fixture.module,
            out: &fixture.out,
        })
        .expect_err("must refuse");
        assert!(err.contains("WebAssembly"), "{err}");
        assert!(!fixture.out.exists(), "nothing may be written on refusal");
    }

    #[test]
    fn packaging_refuses_an_oversized_input_before_reading_it() {
        // `fs::read` sizes its buffer from the file, so a ceiling applied after
        // it returns is a decision made after the damage. Sparse files, so the
        // test does not allocate what it is proving is refused.
        for oversize_manifest in [true, false] {
            let fixture = good_fixture();
            let target = if oversize_manifest {
                &fixture.manifest
            } else {
                &fixture.module
            };
            let file = std::fs::File::create(target).expect("creates");
            file.set_len(128 * 1024 * 1024).expect("sets len");
            drop(file);

            let err = package(&PackageOptions {
                manifest: &fixture.manifest,
                module: &fixture.module,
                out: &fixture.out,
            })
            .expect_err("must refuse");
            assert!(
                err.contains("ceiling"),
                "expected a size refusal, got a {}-byte message starting {:?}",
                err.len(),
                err.chars().take(120).collect::<String>()
            );
            assert!(!fixture.out.exists(), "nothing may be written on refusal");
        }
    }

    #[test]
    fn packaging_refuses_a_manifest_that_does_not_validate() {
        let fixture = fixture(
            &manifest_toml().replace(r#"prefix = "/hello""#, r#"prefix = "/""#),
            wat::parse_str(MODULE).expect("valid WAT"),
        );
        let err = package(&PackageOptions {
            manifest: &fixture.manifest,
            module: &fixture.module,
            out: &fixture.out,
        })
        .expect_err("must refuse");
        assert!(err.contains("prefix"), "{err}");
    }

    #[test]
    fn packaging_refuses_a_module_the_sandbox_could_never_run() {
        // Better to fail at packaging than at the operator's boot.
        let module = wat::parse_str(
            r#"(module
                 (import "autumn_db" "query" (func (param i32) (result i32)))
                 (memory (export "memory") 1)
                 (func (export "_start") (nop)))"#,
        )
        .expect("valid WAT");
        let fixture = fixture(&manifest_toml(), module);
        let err = package(&PackageOptions {
            manifest: &fixture.manifest,
            module: &fixture.module,
            out: &fixture.out,
        })
        .expect_err("must refuse");
        assert!(err.contains("autumn_db"), "{err}");
    }

    #[test]
    fn the_inspect_report_is_a_consent_screen() {
        let fixture = good_fixture();
        let artifact = pack(&fixture);
        let report = Report::of(&artifact);
        let text = report.to_text();
        for expected in [
            "autumn-plugin-hello",
            "/hello",
            "http-request",
            "GET /hello/greet",
            &artifact.manifest().sha256,
            "filesystem",
            "wasi_snapshot_preview1::fd_write",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
    }

    #[test]
    fn the_inspect_report_runs_the_conformance_checks_with_no_binary_to_build() {
        let artifact = pack(&good_fixture());
        let report = Report::of(&artifact);
        assert!(report.loads);
        assert!(
            report.conformance.passed(),
            "{}",
            report.conformance.to_text_report()
        );
        assert!(report.passed());
    }

    #[test]
    fn an_artifact_without_a_recordable_identity_does_not_pass() {
        let artifact = pack(&good_fixture());
        let mut report = Report::of(&artifact);
        assert!(report.passed(), "the fixture should pass to begin with");

        // `artifact_digest()` fails when the container cannot be re-rendered —
        // a manifest that fits the size limit on disk can exceed it once
        // written back in canonical form — and `Report::of` turns that into
        // `None` rather than papering over it. But reporting it is not the same
        // as failing on it: `SandboxedPlugin::from_artifact` computes the same
        // digest and refuses, so a report that still passed said "fit to
        // install" about an artifact the runtime rejects. It is also the number
        // the consent screen asks the operator to record, and there is nothing
        // to record when it is missing.
        report.artifact_sha256 = None;
        assert!(
            !report.passed(),
            "a report with no artifact identity must not pass",
        );
    }

    #[test]
    fn the_json_report_carries_what_a_reviewer_would_diff() {
        let artifact = pack(&good_fixture());
        let json = Report::of(&artifact).to_json().expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parses");
        assert_eq!(value["name"], "autumn-plugin-hello");
        assert_eq!(value["prefix"], "/hello");
        assert_eq!(value["sha256"], artifact.manifest().sha256.as_str());
        assert_eq!(value["capabilities"][0], "http-request");
        assert_eq!(value["routes"][0]["path"], "/hello/greet");
        assert_eq!(value["loads"], true);
        assert!(value["denied"].as_array().is_some_and(|d| !d.is_empty()));
    }

    #[test]
    fn both_review_surfaces_carry_the_identity_that_covers_the_grant() {
        // The guide tells an operator to record the digest this screen printed
        // and compare it against what the deployment loads. The module digest
        // cannot carry that: the grant being reviewed — prefix, routes,
        // capabilities, ceilings — is all manifest, and rewriting it leaves the
        // module digest correct, because the module really did not change.
        let artifact = pack(&good_fixture());
        let identity = artifact.artifact_digest().expect("the artifact re-renders");
        let report = Report::of(&artifact);

        let text = report.to_text();
        assert!(
            text.contains(&identity),
            "the text review surface does not print the identity:\n{text}",
        );
        assert!(
            text.contains("record this one"),
            "the screen must say which of the two digests to keep:\n{text}",
        );

        let json = report.to_json().expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parses");
        assert_eq!(value["artifact_sha256"], identity.as_str());
        // Both are reported, because they answer different questions and the
        // narrower one is still what the loader verifies.
        assert_eq!(value["sha256"], artifact.manifest().sha256.as_str());
        assert_ne!(
            value["artifact_sha256"], value["sha256"],
            "an identity equal to the module digest would not cover the manifest",
        );
    }

    #[test]
    fn the_json_report_names_the_head_a_get_route_also_serves() {
        // The JSON report is the surface an automated consent or policy check
        // reads. HTTP serves HEAD wherever it serves GET and the runtime mounts
        // it, so a manifest declaring only GET — which this fixture does — still
        // answers HEAD. Listing only the literal manifest entries let a machine
        // approve a route set smaller than the artifact actually serves, while
        // the human surfaces (`consent_summary`, `autumn routes`) named it.
        let artifact = pack(&good_fixture());
        let json = Report::of(&artifact).to_json().expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parses");
        let routes = value["routes"].as_array().expect("routes is an array");

        let pairs: Vec<(String, String)> = routes
            .iter()
            .map(|route| {
                (
                    route["method"].as_str().unwrap_or_default().to_owned(),
                    route["path"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();

        assert!(
            pairs.contains(&("GET".to_owned(), "/hello/greet".to_owned())),
            "the declared route must still be reported: {pairs:?}",
        );
        assert!(
            pairs.contains(&("HEAD".to_owned(), "/hello/greet".to_owned())),
            "the implied HEAD is reachable but unreported: {pairs:?}",
        );
        // And the text surface agrees, so the two cannot drift apart.
        assert!(
            Report::of(&artifact).to_text().contains("HEAD"),
            "the human surface stopped naming the implied HEAD",
        );
    }

    #[test]
    fn the_json_report_bounds_artifact_strings_the_way_the_text_one_does() {
        // The text renderer excerpted; `to_json` serialized the report's own
        // fields, so the bound stopped at whichever renderer remembered it.
        //
        // The wasm parser caps one name near 100 KiB, so a single import cannot
        // fill the module allowance — but the reported list holds up to 256 of
        // them, and `serde_json` expands every control character again escaping
        // it. The bound belongs on the report either way: which renderer is
        // safe should not depend on which one remembered to call `excerpt`.
        let name = format!("\\1b[2K{}", "a".repeat(50_000));
        let module = wat::parse_str(format!(
            r#"(module
                 (import "env" "{name}" (func))
                 (memory (export "memory") 1)
                 (func (export "_start") (nop)))"#
        ))
        .expect("valid WAT");
        let artifact = SandboxArtifact::seal(
            SandboxManifest::parse(&manifest_toml()).expect("valid manifest"),
            module,
        )
        .expect("seals");

        let built = Report::of(&artifact);
        let json = built.to_json().expect("serializes");
        assert!(
            json.len() < 64 * 1024,
            "the whole name reached the JSON: {} bytes",
            json.len()
        );
        // Bounded, and still an honest report: the reader must be able to see
        // that it was cut rather than that the artifact was small.
        assert!(json.contains("truncated"), "the cut is not disclosed");
        assert!(
            !json.contains('\u{1b}'),
            "a raw escape reached a consumer that may render it"
        );
    }

    #[test]
    fn inspecting_a_tampered_artifact_refuses_with_a_reason() {
        let fixture = good_fixture();
        pack(&fixture);
        let mut bytes = std::fs::read(&fixture.out).expect("reads");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&fixture.out, &bytes).expect("writes");

        let err = inspect(&fixture.out).expect_err("must refuse");
        assert!(err.contains("digest mismatch"), "{err}");
    }

    #[test]
    fn a_report_for_a_module_that_cannot_load_fails_rather_than_warns() {
        // Packaging refuses these, but an artifact built by an older or
        // friendlier packer must still not pass inspection here.
        let module = wat::parse_str(
            r#"(module
                 (import "env" "system" (func (param i32) (result i32)))
                 (memory (export "memory") 1)
                 (func (export "_start") (nop)))"#,
        )
        .expect("valid WAT");
        // `seal` binds and stamps; it does not judge what the module imports.
        // That judgement is the report's, and it must be a refusal.
        let artifact = SandboxArtifact::seal(
            SandboxManifest::parse(&manifest_toml()).expect("valid manifest"),
            module,
        )
        .expect("seals");
        let report = Report::of(&artifact);
        assert!(!report.loads);
        assert!(!report.passed());
        assert!(
            report.to_text().contains("env::system"),
            "{}",
            report.to_text()
        );
    }

    #[test]
    fn a_module_refused_for_its_shape_reports_unread_imports_not_no_imports() {
        // The consent screen exists to answer "what authority does this ask
        // for?", and the artifacts where that answer matters most are the ones
        // the sandbox already refuses. Enumeration is refused with them — the
        // shape ceiling fires before the module is compiled — and reporting
        // that as an empty list made the screen answer "none" for an artifact
        // refused *for how much it imports*.
        let imports = (0..=autumn_web::plugin_sandbox::host::MAX_IMPORTS)
            .map(|_| {
                r#"  (import "wasi_snapshot_preview1" "fd_write"
                       (func (param i32 i32 i32 i32) (result i32)))"#
            })
            .collect::<Vec<_>>()
            .join("\n");
        let module = wat::parse_str(format!(
            "(module\n{imports}\n  (memory (export \"memory\") 1)\n  (func (export \"_start\") (nop)))"
        ))
        .expect("valid WAT");
        let artifact = SandboxArtifact::seal(
            SandboxManifest::parse(&manifest_toml()).expect("valid manifest"),
            module,
        )
        .expect("seals");

        let report = Report::of(&artifact);
        assert!(!report.loads, "the fixture must be refused to be the case");
        assert!(
            report.imports.is_none(),
            "an unreadable list must not be recorded as an empty one: {:?}",
            report.imports
        );

        let text = report.to_text();
        assert!(
            !text.contains("(none)"),
            "the screen still claims it asks for nothing:\n{text}"
        );
        assert!(text.contains("could not be read"), "{text}");

        // …and the JSON surface agrees, since a reviewer diffing reports reads
        // that one instead. `null` is distinguishable from `[]`; `[]` is not
        // distinguishable from the truth.
        let json = report.to_json().expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parses");
        assert!(value["imports"].is_null(), "{}", value["imports"]);
    }
}
