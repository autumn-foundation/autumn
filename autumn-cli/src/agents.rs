//! `autumn agents manifest` — emit the agent-authority manifest (issue #1691).
//!
//! Runs the app's own binary in agent-authority dump mode
//! (`AUTUMN_DUMP_AGENT_AUTHORITY=1`), reads back the manifest the framework
//! assembles from every `#[agent_operable]` action and every declared
//! `authority_grant!` it links, joins it against the app's route table, and
//! writes it out as a build artifact.
//!
//! Why run the binary rather than parse the sources: which handlers are
//! agent-operable — and which of them an agent can actually *reach* — is a
//! whole-app fact. An action declared in a plugin the app merely depends on is
//! still an action an agent can take, and link-time `inventory` collection is
//! the only place all of those registrations exist together. Same shape as
//! `autumn data-flow` (#1654), `autumn cache audit` (#1716) and
//! `autumn routes audit` (#1604).
//!
//! Unlike `data-flow`, this command *does* have a gate of its own. The compiler
//! is still the gate for everything a grant covers — an unlisted write does not
//! build — but a tool with **no** grant at all is invisible to that gate by
//! construction. `--check` therefore fails on any MCP-exposed *mutating* tool
//! with no envelope unless `--allow-ungoverned` is passed, and warns about the
//! read-only ones.
//!
//! See `docs/guide/agent-authority.md`.

use std::process::Command;

use autumn_web::agent_authority::manifest::{
    AgentAuthorityManifest, UngovernedTool, parse_manifest_dump,
};

use crate::routes;

/// The env var selecting the app binary's agent-authority dump mode.
///
/// Named rather than spelled out at each site because it is set in one place
/// and must be *cleared* in every other place that spawns the app binary:
/// `AppBuilder::run` dispatches this mode after the build, route, cache and
/// data-flow one-shots and before the server binds a listener, so an inherited
/// value silently wins over whatever was actually asked for.
pub const DUMP_ENV: &str = "AUTUMN_DUMP_AGENT_AUTHORITY";

/// Options controlling `autumn agents manifest`.
pub struct AgentsManifestOptions<'a> {
    /// Cargo package to build and run.
    pub package: Option<&'a str>,
    /// Binary target name for packages that expose multiple bin targets.
    pub bin: Option<&'a str>,
    /// Write the JSON manifest to this path (in addition to any stdout).
    pub manifest: Option<&'a str>,
    /// Emit the JSON manifest to stdout instead of the human report.
    pub json: bool,
    /// Compare against a committed manifest and fail on drift.
    pub check: Option<&'a str>,
    /// Let `--check` pass with MCP-exposed mutating tools that carry no
    /// authority envelope.
    ///
    /// The escape hatch exists because an app can adopt this incrementally, and
    /// because `#[repository(api, mcp)]` generates CRUD tools that have no
    /// annotation site in this slice. It is a flag, not a default: a mutating
    /// tool an agent can call with no declared envelope is the thing this
    /// command exists to surface.
    pub allow_ungoverned: bool,
    /// Cargo feature selection the inspected binary is built under.
    ///
    /// The manifest describes the binary that produced it. An action or a grant
    /// behind a non-default feature is simply not compiled in, so it cannot
    /// appear in the manifest.
    pub features: routes::CargoFeatures,
    /// Build and inspect the release binary rather than the debug one.
    pub release: bool,
}

/// Render the human report for a manifest.
#[must_use]
pub fn format_report(manifest: &AgentAuthorityManifest) -> String {
    // RED STUB (#1691).
    let _ = manifest;
    String::new()
}

/// Describe the difference between a committed manifest and a fresh one.
///
/// Returns `None` when they agree. The report names *which* rows moved, because
/// "the manifest changed" is not reviewable but "`draft_refund` gained the
/// effect `outbound https://api.stripe.com/v1/charges`" is.
#[must_use]
pub fn format_drift(
    committed: &AgentAuthorityManifest,
    current: &AgentAuthorityManifest,
) -> Option<String> {
    // RED STUB (#1691).
    let _ = (committed, current);
    None
}

/// The `--check` gate on tools nothing governs.
///
/// Returns the failure message when the run must exit non-zero, and `None` when
/// it may pass (possibly after a warning, which
/// [`format_ungoverned_warning`] renders).
#[must_use]
pub fn format_ungoverned_failure(
    manifest: &AgentAuthorityManifest,
    allow_ungoverned: bool,
) -> Option<String> {
    // RED STUB (#1691).
    let _ = (manifest, allow_ungoverned);
    None
}

/// The advisory half: read-only tools with no envelope, and mutating ones the
/// run was explicitly told to allow.
#[must_use]
pub fn format_ungoverned_warning(manifest: &AgentAuthorityManifest) -> Option<String> {
    // RED STUB (#1691).
    let _ = manifest;
    None
}

/// One ungoverned tool, as the report names it.
// RED (#1691): only the tests reach this while the report and the gate are
// stubs; the green phase calls it from both and the allow comes off.
#[allow(dead_code)]
#[must_use]
pub fn describe_tool(tool: &UngovernedTool) -> String {
    format!("{} {} ({})", tool.method, tool.path, tool.tool)
}

/// Write the manifest JSON to `path`.
///
/// # Errors
///
/// Returns the underlying I/O error when the file cannot be written.
pub fn write_manifest(
    manifest: &AgentAuthorityManifest,
    path: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::write(path, format!("{}\n", manifest.to_json()))
}

/// Run `autumn agents manifest`.
pub fn run(opts: &AgentsManifestOptions<'_>) {
    eprintln!("\u{1F342} autumn agents manifest\n");
    if !opts.features.is_default() {
        eprintln!("Building with {}\n", opts.features.to_args().join(" "));
    }
    if opts.release {
        eprintln!("Building the release profile\n");
    }
    routes::compile_binary_with_profile(opts.package, opts.bin, &opts.features, opts.release);
    let binary = routes::find_binary_in_profile(opts.package, opts.bin, opts.release);

    let output = Command::new(&binary)
        .env(DUMP_ENV, "1")
        // Every one of these is checked BEFORE the agent-authority dump in
        // `AppBuilder::run`, so an exported one in the ambient environment
        // would silently win and hand us a marker-less stdout.
        .env_remove("AUTUMN_BUILD_STATIC")
        .env_remove("AUTUMN_DUMP_ROUTES")
        .env_remove("AUTUMN_DUMP_CACHE_COHERENCE")
        .env_remove("AUTUMN_DUMP_DATA_FLOW")
        .env_remove("AUTUMN_DUMP_JOBS")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    eprint!("{}", String::from_utf8_lossy(&output.stderr));

    if !output.status.success() {
        eprintln!(
            "\u{2717} Binary exited with status {} while dumping the agent-authority manifest",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(manifest) = parse_manifest_dump(&stdout) else {
        eprintln!(
            "\u{2717} The app produced no agent-authority manifest. Either it was built against \
             an autumn-web without `autumn agents manifest` support, or it took a different \
             startup path first \u{2014} `AUTUMN_BUILD_STATIC`, `AUTUMN_DUMP_ROUTES`, \
             `AUTUMN_DUMP_CACHE_COHERENCE`, `AUTUMN_DUMP_DATA_FLOW` and `AUTUMN_DUMP_JOBS` are \
             all handled before the manifest dump and are cleared for this run, so an app that \
             exits earlier for its own reasons will land here too."
        );
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    };

    // Read the committed copy BEFORE writing anything. `--manifest P --check P`
    // would otherwise compare the fresh manifest against the copy this very run
    // just wrote and always pass -- a gate that certifies itself.
    let committed = opts.check.map(|path| match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<AgentAuthorityManifest>(&text) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("\u{2717} {path} is not an agent-authority manifest: {e}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("\u{2717} Failed to read {path}: {e}");
            std::process::exit(1);
        }
    });

    if let Some(path) = opts.manifest {
        if let Err(e) = write_manifest(&manifest, std::path::Path::new(path)) {
            eprintln!("\u{2717} Failed to write manifest to {path}: {e}");
            std::process::exit(1);
        }
        eprintln!("\u{2713} Wrote agent-authority manifest \u{2192} {path}");
    }

    if opts.json {
        println!("{}", manifest.to_json());
    } else {
        println!("{}", format_report(&manifest));
    }

    if let Some(warning) = format_ungoverned_warning(&manifest) {
        eprintln!("{warning}");
    }

    let mut failed = false;

    if let (Some(path), Some(committed)) = (opts.check, committed) {
        if let Some(drift) = format_drift(&committed, &manifest) {
            eprintln!("{drift}");
            eprintln!(
                "\nIf the change is intended, re-run with `--manifest {path}` and commit the result."
            );
            failed = true;
        } else {
            eprintln!("\u{2713} The agent-authority manifest matches {path}.");
        }

        if let Some(ungoverned) = format_ungoverned_failure(&manifest, opts.allow_ungoverned) {
            eprintln!("{ungoverned}");
            failed = true;
        }
    }

    if failed {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use autumn_web::agent_authority::manifest::RouteSummary;
    use autumn_web::agent_authority::{
        AgentAuthority, Effect, EffectKind, EffectProvenance, Grant, Reversibility, TenantScope,
    };

    use super::*;

    // ── Fixtures ─────────────────────────────────────────────────────

    static REFUND_GRANT: Grant = Grant {
        name: "RefundDrafter",
        writes: &["Refund"],
        unbounded_writes: &[],
        tenant_scope: TenantScope::Scoped,
        outbound: &["https://api.stripe.com/v1/refunds"],
        webhooks: &[],
        jobs: &[],
        rate: Some("10/min"),
        spend: Some("500.00 USD"),
        reversibility: Reversibility::Compensable,
        location: "billing/refunds.rs:10",
    };

    static WRITE_ONLY: &[Effect] = &[Effect {
        kind: EffectKind::Write,
        subject: "Refund",
        location: "billing/refunds.rs:30",
        provenance: EffectProvenance::TypeResolved,
    }];

    static WRITE_AND_OUTBOUND: &[Effect] = &[
        Effect {
            kind: EffectKind::Write,
            subject: "Refund",
            location: "billing/refunds.rs:30",
            provenance: EffectProvenance::TypeResolved,
        },
        Effect {
            kind: EffectKind::Outbound,
            subject: "https://api.stripe.com/v1/refunds",
            location: "billing/refunds.rs:42",
            provenance: EffectProvenance::Syntactic,
        },
    ];

    static DRAFT_REFUND: AgentAuthority = AgentAuthority {
        action: "draft_refund",
        module_path: "billing::refunds",
        location: "billing/refunds.rs:28",
        grant: &REFUND_GRANT,
        effects: WRITE_ONLY,
        asserted_effect_free_sites: 0,
    };

    static DRAFT_REFUND_WIDENED: AgentAuthority = AgentAuthority {
        action: "draft_refund",
        module_path: "billing::refunds",
        location: "billing/refunds.rs:28",
        grant: &REFUND_GRANT,
        effects: WRITE_AND_OUTBOUND,
        asserted_effect_free_sites: 0,
    };

    static ARCHIVE_REFUND: AgentAuthority = AgentAuthority {
        action: "archive_refund",
        module_path: "billing::refunds",
        location: "billing/refunds.rs:70",
        grant: &REFUND_GRANT,
        effects: WRITE_ONLY,
        asserted_effect_free_sites: 0,
    };

    fn route(
        method: &str,
        path: &str,
        handler: &'static str,
        module_path: &'static str,
        mcp_tool: bool,
        authority: Option<&'static AgentAuthority>,
    ) -> RouteSummary {
        RouteSummary {
            method: method.to_string(),
            path: path.to_string(),
            handler,
            module_path,
            mcp_tool,
            agent_authority: authority,
        }
    }

    fn governed_route(authority: &'static AgentAuthority) -> RouteSummary {
        route(
            "POST",
            "/refunds",
            authority.action,
            authority.module_path,
            true,
            Some(authority),
        )
    }

    fn manifest_of(
        authorities: &[&'static AgentAuthority],
        routes: &[RouteSummary],
    ) -> AgentAuthorityManifest {
        AgentAuthorityManifest::from_parts(authorities, &[&REFUND_GRANT], routes, true)
    }

    // ── The human report ─────────────────────────────────────────────

    #[test]
    fn the_report_names_each_action_its_envelope_and_its_effects() {
        let manifest = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        let report = format_report(&manifest);
        assert!(report.contains("draft_refund"), "{report}");
        assert!(report.contains("RefundDrafter"), "{report}");
        assert!(report.contains("compensable"), "{report}");
        assert!(report.contains("Refund"), "{report}");
    }

    #[test]
    fn the_report_states_that_the_caps_are_declared_only() {
        // A reader who sees `rate: 10/min` in a manifest will assume something
        // enforces it. Nothing does, in this slice, and the report is where
        // that has to be said.
        let manifest = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        let report = format_report(&manifest);
        assert!(report.contains("declared"), "{report}");
        assert!(report.contains("not enforced"), "{report}");
    }

    // ── Drift ────────────────────────────────────────────────────────

    #[test]
    fn an_identical_manifest_reports_no_drift() {
        let manifest = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        assert!(format_drift(&manifest, &manifest).is_none());
    }

    #[test]
    fn a_new_action_is_named_in_the_drift_report() {
        let before = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        let after = manifest_of(
            &[&DRAFT_REFUND, &ARCHIVE_REFUND],
            &[
                governed_route(&DRAFT_REFUND),
                governed_route(&ARCHIVE_REFUND),
            ],
        );
        let drift = format_drift(&before, &after).expect("drift");
        assert!(
            drift.contains("+ agent-operable action billing::refunds::archive_refund"),
            "{drift}"
        );
    }

    #[test]
    fn a_removed_action_is_named_in_the_drift_report() {
        let before = manifest_of(
            &[&DRAFT_REFUND, &ARCHIVE_REFUND],
            &[
                governed_route(&DRAFT_REFUND),
                governed_route(&ARCHIVE_REFUND),
            ],
        );
        let after = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        let drift = format_drift(&before, &after).expect("drift");
        assert!(
            drift.contains("- agent-operable action billing::refunds::archive_refund"),
            "{drift}"
        );
    }

    #[test]
    fn a_widened_effect_set_is_named_effect_by_effect() {
        // "the manifest changed" is not reviewable. "`draft_refund` gained
        // `outbound https://api.stripe.com/v1/refunds`" is exactly the line a
        // reviewer needs, so the report has to name the effect, not the row.
        let before = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        let after = manifest_of(
            &[&DRAFT_REFUND_WIDENED],
            &[governed_route(&DRAFT_REFUND_WIDENED)],
        );
        let drift = format_drift(&before, &after).expect("drift");
        assert!(drift.contains("billing::refunds::draft_refund"), "{drift}");
        assert!(drift.contains("+ outbound"), "{drift}");
        assert!(
            drift.contains("https://api.stripe.com/v1/refunds"),
            "{drift}"
        );
        // And the other direction names the loss.
        let narrowed = format_drift(&after, &before).expect("drift");
        assert!(narrowed.contains("- outbound"), "{narrowed}");
    }

    #[test]
    fn a_schema_version_change_is_reported() {
        let before = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        let mut after = before.clone();
        after.schema_version += 1;
        let drift = format_drift(&before, &after).expect("drift");
        assert!(drift.contains("schema version"), "{drift}");
    }

    #[test]
    fn a_newly_ungoverned_tool_is_named_in_the_drift_report() {
        let before = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        let after = manifest_of(
            &[&DRAFT_REFUND],
            &[
                governed_route(&DRAFT_REFUND),
                route(
                    "DELETE",
                    "/widgets/{id}",
                    "destroy_widget",
                    "shop::widgets",
                    true,
                    None,
                ),
            ],
        );
        let drift = format_drift(&before, &after).expect("drift");
        assert!(drift.contains("destroy_widget"), "{drift}");
        assert!(drift.contains("ungoverned"), "{drift}");
    }

    // ── The ungoverned-tool gate ─────────────────────────────────────

    #[test]
    fn a_mutating_tool_with_no_envelope_fails_the_check() {
        let manifest = manifest_of(
            &[&DRAFT_REFUND],
            &[
                governed_route(&DRAFT_REFUND),
                route(
                    "DELETE",
                    "/widgets/{id}",
                    "destroy_widget",
                    "shop::widgets",
                    true,
                    None,
                ),
            ],
        );
        let failure = format_ungoverned_failure(&manifest, false)
            .expect("a mutating tool nothing governs must fail the gate");
        assert!(failure.contains("destroy_widget"), "{failure}");
        assert!(failure.contains("DELETE /widgets/{id}"), "{failure}");
        // The message has to say how to get past it, or the flag is folklore.
        assert!(failure.contains("--allow-ungoverned"), "{failure}");
        assert!(failure.contains("agent_operable"), "{failure}");
    }

    #[test]
    fn the_allow_flag_turns_the_failure_into_a_pass() {
        let manifest = manifest_of(
            &[&DRAFT_REFUND],
            &[
                governed_route(&DRAFT_REFUND),
                route(
                    "DELETE",
                    "/widgets/{id}",
                    "destroy_widget",
                    "shop::widgets",
                    true,
                    None,
                ),
            ],
        );
        assert!(format_ungoverned_failure(&manifest, true).is_none());
        // ... but it is still reported: allowed is not the same as unseen.
        let warning = format_ungoverned_warning(&manifest).expect("warning");
        assert!(warning.contains("destroy_widget"), "{warning}");
    }

    #[test]
    fn a_read_only_tool_with_no_envelope_warns_but_does_not_fail() {
        let manifest = manifest_of(
            &[&DRAFT_REFUND],
            &[
                governed_route(&DRAFT_REFUND),
                route(
                    "GET",
                    "/widgets",
                    "list_widgets",
                    "shop::widgets",
                    true,
                    None,
                ),
            ],
        );
        assert!(format_ungoverned_failure(&manifest, false).is_none());
        let warning = format_ungoverned_warning(&manifest).expect("warning");
        assert!(warning.contains("list_widgets"), "{warning}");
    }

    #[test]
    fn a_fully_governed_app_neither_fails_nor_warns() {
        let manifest = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        assert!(format_ungoverned_failure(&manifest, false).is_none());
        assert!(format_ungoverned_warning(&manifest).is_none());
    }

    #[test]
    fn a_tool_is_described_by_method_path_and_name() {
        let manifest = manifest_of(
            &[],
            &[route(
                "DELETE",
                "/widgets/{id}",
                "destroy_widget",
                "shop::widgets",
                true,
                None,
            )],
        );
        let tool = manifest
            .ungoverned_tools
            .first()
            .expect("the route is an ungoverned tool");
        assert_eq!(describe_tool(tool), "DELETE /widgets/{id} (destroy_widget)");
    }

    // ── Round trip ───────────────────────────────────────────────────

    #[test]
    fn the_manifest_written_to_disk_reads_back_as_the_same_document() {
        let manifest = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        let dir = std::env::temp_dir().join(format!(
            "autumn-agent-authority-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("agent-authority-manifest.json");
        write_manifest(&manifest, &path).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        let parsed: AgentAuthorityManifest = serde_json::from_str(&text).expect("parse");
        assert_eq!(parsed, manifest);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
