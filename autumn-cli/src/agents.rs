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
//! Unlike `data-flow`, this command *does* have gates of its own — and
//! `--check` is where they live, which makes it the CI invocation. The compiler
//! is still the gate for everything a grant covers — an unlisted write does not
//! build — but three things are invisible to it by construction, so `--check`
//! fails on:
//!
//! * an MCP-exposed **mutating** tool with no envelope (unless
//!   `--allow-ungoverned`), because a tool with no grant has no const assertion
//!   to fail;
//! * a binary with **no audit sink** that can still take an action nothing can
//!   undo (unless `--allow-unaudited`) — with no sink installed the audit write
//!   trivially succeeds, so the runtime's fail-closed refusal never fires and
//!   the invocation leaves no trace at all;
//! * a route naming an **authority nothing registered**, which would otherwise
//!   appear in no list and so in no gate;
//! * and drift from the committed manifest.
//!
//! See `docs/guide/agent-authority.md`.

use std::process::Command;

use autumn_web::agent_authority::manifest::{
    ActionRow, AgentAuthorityManifest, UngovernedTool, UnregisteredAuthority, parse_manifest_dump,
};

use crate::routes;

/// The env var selecting the app binary's agent-authority dump mode.
///
/// Named rather than spelled out at each site because it is set in one place
/// and must be *cleared* in every other place that spawns the app binary:
/// `AppBuilder::run` dispatches this mode after the build, route, cache and
/// data-flow one-shots and before the server binds a listener, so an inherited
/// value silently wins over whatever was actually asked for.
///
/// Re-exported from the framework rather than spelled again here: the CLI sets
/// this string and `AppBuilder::run` reads it, so two independent literals
/// would be a protocol that could silently drift apart at a typo.
pub const DUMP_ENV: &str = autumn_web::agent_authority::manifest::DUMP_ENV;

/// Options controlling `autumn agents manifest`.
#[allow(clippy::struct_excessive_bools)] // independent CLI flags, not a state machine
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
    /// Let `--check` pass with no audit sink configured even though the binary
    /// can take an action nothing can undo.
    ///
    /// The twin of [`Self::allow_ungoverned`], and a flag for the same reason:
    /// a development binary legitimately has no sink. What it must not be is
    /// the default, because this is the one combination the *runtime* cannot
    /// catch — `write_from_state` returns `Ok(())` when no logger is installed,
    /// so the attempt record "succeeds", the fail-closed refusal never fires,
    /// and nothing is recorded anywhere.
    pub allow_unaudited: bool,
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
///
/// The manifest's own summary: the CLI has no second opinion about what the
/// document says, and two renderings that could disagree is exactly the drift
/// these commands exist to catch.
#[must_use]
pub fn format_report(manifest: &AgentAuthorityManifest) -> String {
    manifest.summary()
}

/// Describe the difference between a committed manifest and a fresh one.
///
/// Returns `None` when they agree. The report names *which* rows moved and
/// *which* effects they gained, because "the manifest changed" is not
/// reviewable but "`draft_refund` gained `outbound https://api.stripe.com/…`"
/// is the one line a reviewer needs.
#[must_use]
pub fn format_drift(
    committed: &AgentAuthorityManifest,
    current: &AgentAuthorityManifest,
) -> Option<String> {
    if committed == current {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();

    if committed.schema_version != current.schema_version {
        lines.push(format!(
            "  manifest schema version {} -> {}",
            committed.schema_version, current.schema_version
        ));
    }

    for row in &current.actions {
        match committed
            .actions
            .iter()
            .find(|c| action_key(c) == action_key(row))
        {
            None => {
                lines.push(format!(
                    "  + agent-operable action {} under grant {}",
                    action_key(row),
                    row.grant.name
                ));
                for effect in &row.effects {
                    lines.push(format!("      + {} {}", effect.kind, effect.subject));
                }
            }
            Some(before) if before != row => {
                lines.push(format!("  ~ {}", action_key(row)));
                lines.extend(action_changes(before, row));
            }
            Some(_) => {}
        }
    }
    for row in &committed.actions {
        if !current
            .actions
            .iter()
            .any(|c| action_key(c) == action_key(row))
        {
            lines.push(format!("  - agent-operable action {}", action_key(row)));
        }
    }

    lines.extend(tool_changes(committed, current));
    lines.extend(unregistered_changes(committed, current));
    lines.extend(grant_changes(committed, current));

    if committed.audit != current.audit {
        lines.push(format!(
            "  agent audit sink configured {} -> {}",
            committed.audit.sink_configured, current.audit.sink_configured
        ));
    }

    if lines.is_empty() {
        lines.push("  (the documents differ in a field this report does not name)".to_string());
    }
    Some(format!(
        "\u{2717} The agent-authority manifest has drifted from the committed copy:\n{}",
        lines.join("\n")
    ))
}

/// Ungoverned MCP tools that appeared or disappeared.
fn tool_changes(
    committed: &AgentAuthorityManifest,
    current: &AgentAuthorityManifest,
) -> Vec<String> {
    let mut lines = Vec::new();
    for tool in &current.ungoverned_tools {
        if !committed.ungoverned_tools.contains(tool) {
            lines.push(format!(
                "  + ungoverned MCP tool {} [{}]{}",
                describe_tool(tool),
                tool.exposed_by,
                if tool.mutating { " -- MUTATING" } else { "" }
            ));
        }
    }
    for tool in &committed.ungoverned_tools {
        if !current.ungoverned_tools.contains(tool) {
            lines.push(format!("  - ungoverned MCP tool {}", describe_tool(tool)));
        }
    }
    lines
}

/// Routes naming an unregistered authority that appeared or disappeared.
fn unregistered_changes(
    committed: &AgentAuthorityManifest,
    current: &AgentAuthorityManifest,
) -> Vec<String> {
    let mut lines = Vec::new();
    for row in &current.unregistered_authorities {
        if !committed.unregistered_authorities.contains(row) {
            lines.push(format!(
                "  + route naming an unregistered authority {}",
                describe_unregistered(row)
            ));
        }
    }
    for row in &committed.unregistered_authorities {
        if !current.unregistered_authorities.contains(row) {
            lines.push(format!(
                "  - route naming an unregistered authority {}",
                describe_unregistered(row)
            ));
        }
    }
    lines
}

/// Declared envelopes that appeared or disappeared.
///
/// Keyed on `(name, location)`, never the name alone: two crates can each
/// declare a `RefundDrafter`, and collapsing them would hide one appearing.
fn grant_changes(
    committed: &AgentAuthorityManifest,
    current: &AgentAuthorityManifest,
) -> Vec<String> {
    let key_missing = |haystack: &AgentAuthorityManifest, name: &str, location: &str| {
        !haystack
            .grants
            .iter()
            .any(|g| g.name == name && g.location == location)
    };
    let mut lines = Vec::new();
    for grant in &current.grants {
        if key_missing(committed, &grant.name, &grant.location) {
            lines.push(format!("  + grant {} ({})", grant.name, grant.location));
        }
    }
    for grant in &committed.grants {
        if key_missing(current, &grant.name, &grant.location) {
            lines.push(format!("  - grant {} ({})", grant.name, grant.location));
        }
    }
    lines
}

/// The `--check` gate on tools nothing governs.
///
/// Returns the failure message when the run must exit non-zero, and `None` when
/// it may pass (possibly after a warning, which
/// [`format_ungoverned_warning`] renders).
///
/// This is the one gate the compiler cannot supply. Everything a grant covers
/// is const-asserted at the call site, but a tool with *no* grant has no
/// assertion to fail — it is invisible to the build, and visible only here.
#[must_use]
pub fn format_ungoverned_failure(
    manifest: &AgentAuthorityManifest,
    allow_ungoverned: bool,
) -> Option<String> {
    if allow_ungoverned {
        return None;
    }
    let mutating = manifest.ungoverned_mutating_tools();
    if mutating.is_empty() {
        return None;
    }
    let listed = mutating
        .iter()
        .map(|tool| format!("  {}", describe_tool(tool)))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "\u{2717} {} MCP tool{} can change state with no authority envelope:\n{listed}\n\n\
         An agent can call these, and nothing declares what they are allowed to do. Annotate each \
         handler with `#[agent_operable(grant = YourGrant)]` and declare the envelope with \
         `authority_grant!`, or re-run with `--allow-ungoverned` to record them as they are. \
         See docs/guide/agent-authority.md",
        mutating.len(),
        if mutating.len() == 1 { "" } else { "s" },
    ))
}

/// The advisory half: read-only tools with no envelope, and mutating ones the
/// run was explicitly told to allow.
///
/// Allowed is not the same as unseen, so `--allow-ungoverned` silences the
/// failure and not the list.
#[must_use]
pub fn format_ungoverned_warning(manifest: &AgentAuthorityManifest) -> Option<String> {
    if manifest.ungoverned_tools.is_empty() {
        return None;
    }
    let listed = manifest
        .ungoverned_tools
        .iter()
        .map(|tool| {
            format!(
                "  {} [{}]{}",
                describe_tool(tool),
                tool.exposed_by,
                if tool.mutating {
                    " -- mutating"
                } else {
                    " -- read-only"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "! {} MCP tool{} exposed with no authority envelope:\n{listed}",
        manifest.ungoverned_tools.len(),
        if manifest.ungoverned_tools.len() == 1 {
            ""
        } else {
            "s"
        },
    ))
}

/// The `--check` gate on a binary that can act irreversibly with nothing
/// recording it.
///
/// Returns the failure message when the run must exit non-zero.
///
/// Why this cannot be a runtime check: with no `AuditLogger` installed,
/// `audit::write_from_state` returns `Ok(())` without writing anything, so the
/// MCP dispatcher's `refuse_when_unauditable` rule sees a successful attempt
/// record and refuses nothing. The fail-closed guarantee protects a *configured*
/// sink that is failing; it says nothing about a sink that was never there.
/// Build time is therefore the only place the combination can be caught
/// (#1691 P1-2).
///
/// Both halves are required. A missing sink is survivable when every
/// agent-reachable action is reversible, and a non-reversible action is
/// survivable when it is recorded; the failure is their conjunction.
#[must_use]
pub fn format_unaudited_failure(
    manifest: &AgentAuthorityManifest,
    allow_unaudited: bool,
) -> Option<String> {
    if allow_unaudited || !manifest.unaudited_and_unrecoverable() {
        return None;
    }
    let mut listed: Vec<String> = manifest
        .non_reversible_actions()
        .iter()
        .map(|row| {
            format!(
                "  {}::{} under grant {} ({})",
                row.module_path, row.action, row.grant.name, row.grant.reversibility
            )
        })
        .collect();
    listed.extend(
        manifest
            .ungoverned_mutating_tools()
            .iter()
            .map(|tool| format!("  {} -- mutating, ungoverned", describe_tool(tool))),
    );
    Some(format!(
        "\u{2717} No agent audit sink is configured, and this binary can still take {} action{} \
         that nothing can undo:\n{}\n\n\
         Nothing records these. With no sink installed the audit write trivially succeeds, so the \
         runtime's fail-closed refusal never fires -- the refusal protects a configured sink that \
         is failing, not a missing one. Install a sink with `AppBuilder::with_audit_sink(..)`, \
         make the actions `reversible`, or re-run with `--allow-unaudited` to accept it. \
         See docs/guide/agent-authority.md",
        listed.len(),
        if listed.len() == 1 { "" } else { "s" },
        listed.join("\n"),
    ))
}

/// The `--check` gate on a route naming an authority nothing registered.
///
/// Always fatal, with no allow-flag twin: unlike an ungoverned tool (a real
/// adoption state) this cannot arise from the macros at all —
/// `#[agent_operable]` always emits the static and its `inventory::submit!`
/// together. A route in this list means someone hand-wrote the pair, and the
/// effect is a tool in neither `actions` nor `ungoverned_tools`, and so in no
/// gate (#1691 P3-1).
#[must_use]
pub fn format_unregistered_failure(manifest: &AgentAuthorityManifest) -> Option<String> {
    if manifest.unregistered_authorities.is_empty() {
        return None;
    }
    let listed = manifest
        .unregistered_authorities
        .iter()
        .map(|row| format!("  {}", describe_unregistered(row)))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "\u{2717} {} route{} an authority nothing registered:\n{listed}\n\n\
         The handler carries an `#[agent_operable]` marker but no descriptor reached the manifest, \
         so the action appears in no list and no gate sees it. This cannot happen through the \
         macros -- check for a hand-written `__AUTUMN_AGENT_AUTHORITY_*` static, or for an \
         `#[agent_operable]` behind a `#[cfg]` the route is not behind. \
         See docs/guide/agent-authority.md",
        manifest.unregistered_authorities.len(),
        if manifest.unregistered_authorities.len() == 1 {
            " names"
        } else {
            "s name"
        },
    ))
}

/// One unregistered authority, as the report names it.
#[must_use]
pub fn describe_unregistered(row: &UnregisteredAuthority) -> String {
    format!(
        "{} {} ({}) -> {}::{}{}",
        row.method,
        row.path,
        row.handler,
        row.module_path,
        row.action,
        if row.mcp_tool { " [mcp tool]" } else { "" }
    )
}

/// One ungoverned tool, as the report names it.
///
/// Names the tool as an MCP client calls it, and the handler too when a route
/// renamed one: the reader needs the first to recognise the tool and the
/// second to find the code.
#[must_use]
pub fn describe_tool(tool: &UngovernedTool) -> String {
    if tool.handler == tool.tool {
        format!("{} {} ({})", tool.method, tool.path, tool.tool)
    } else {
        format!(
            "{} {} ({}, handler {})",
            tool.method, tool.path, tool.tool, tool.handler
        )
    }
}

/// What changed between two versions of the same action row.
///
/// Split out of [`format_drift`] because this is the part a reviewer actually
/// reads: an effect appearing on a governed action is the whole point of
/// keeping the manifest under review, and it deserves a line of its own rather
/// than a "row changed" that sends them to `git diff`.
fn action_changes(before: &ActionRow, after: &ActionRow) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    if before.grant.name != after.grant.name {
        lines.push(format!(
            "      grant {} -> {}",
            before.grant.name, after.grant.name
        ));
    }
    if before.grant.reversibility != after.grant.reversibility {
        lines.push(format!(
            "      reversibility {} -> {}",
            before.grant.reversibility, after.grant.reversibility
        ));
    }
    if before.grant.tenant_scope != after.grant.tenant_scope {
        lines.push(format!(
            "      tenant scope {} -> {}",
            before.grant.tenant_scope, after.grant.tenant_scope
        ));
    }
    if before.grant.rate != after.grant.rate {
        lines.push(format!(
            "      declared rate {} -> {}",
            show(before.grant.rate.as_deref()),
            show(after.grant.rate.as_deref())
        ));
    }
    if before.grant.spend != after.grant.spend {
        lines.push(format!(
            "      declared spend {} -> {}",
            show(before.grant.spend.as_deref()),
            show(after.grant.spend.as_deref())
        ));
    }
    for effect in &after.effects {
        if !before.effects.contains(effect) {
            lines.push(format!(
                "      + {} {} ({}, {})",
                effect.kind, effect.subject, effect.provenance, effect.location
            ));
        }
    }
    for effect in &before.effects {
        if !after.effects.contains(effect) {
            lines.push(format!(
                "      - {} {} ({})",
                effect.kind, effect.subject, effect.provenance
            ));
        }
    }
    if before.exposure != after.exposure {
        lines.push(format!(
            "      exposure {} -> {}",
            before.exposure, after.exposure
        ));
    }
    if before.provenance != after.provenance {
        // `provable` -> `declared` means someone wrote an effect down by hand.
        // That is legal, and it is exactly the change a reviewer must see.
        lines.push(format!(
            "      provenance {} -> {}",
            before.provenance, after.provenance
        ));
    }
    // A hatch appearing or disappearing changes what the row *proves*, not
    // what it lists, so nothing else in this diff would show it. That is
    // exactly why it is here: adding `#[agent_effect(none, ...)]` over a
    // statement that really does call out used to produce no drift at all.
    for site in &after.asserted_effect_free {
        if !before.asserted_effect_free.contains(site) {
            lines.push(format!(
                "      + asserted effect-free at {}: {}",
                site.location, site.reason
            ));
        }
    }
    for site in &before.asserted_effect_free {
        if !after.asserted_effect_free.contains(site) {
            lines.push(format!(
                "      - asserted effect-free at {}: {}",
                site.location, site.reason
            ));
        }
    }
    if before.unused_grant_entries != after.unused_grant_entries {
        lines.push(format!(
            "      granted-but-unused: [{}] -> [{}]",
            before.unused_grant_entries.join(", "),
            after.unused_grant_entries.join(", ")
        ));
    }
    lines
}

/// How a row is identified across two manifests: the module path and the
/// handler name, never the handler name alone — two crates can each define a
/// `draft_refund`, and a report that cannot tell them apart would attribute one
/// crate's new effect to the other.
fn action_key(row: &ActionRow) -> String {
    format!("{}::{}", row.module_path, row.action)
}

fn show(value: Option<&str>) -> String {
    value.map_or_else(|| "none".to_string(), ToString::to_string)
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

        // Every gate below is evaluated on the FRESH manifest, never the
        // committed one, so a doctored file can only cause a drift failure --
        // it can never certify away a tool the binary really exposes.
        if let Some(ungoverned) = format_ungoverned_failure(&manifest, opts.allow_ungoverned) {
            eprintln!("{ungoverned}");
            failed = true;
        }

        if let Some(unaudited) = format_unaudited_failure(&manifest, opts.allow_unaudited) {
            eprintln!("{unaudited}");
            failed = true;
        }

        if let Some(unregistered) = format_unregistered_failure(&manifest) {
            eprintln!("{unregistered}");
            failed = true;
        }
    }

    if failed {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use autumn_web::agent_authority::manifest::{
        AssertedEffectFreeRow, McpExposedBy, RouteSummary,
    };
    use autumn_web::agent_authority::{
        AgentAuthority, AssertedEffectFree, Effect, EffectKind, EffectProvenance, Grant,
        Reversibility, TenantScope,
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
        asserted_effect_free: &[],
    };

    static DRAFT_REFUND_WIDENED: AgentAuthority = AgentAuthority {
        action: "draft_refund",
        module_path: "billing::refunds",
        location: "billing/refunds.rs:28",
        grant: &REFUND_GRANT,
        effects: WRITE_AND_OUTBOUND,
        asserted_effect_free_sites: 0,
        asserted_effect_free: &[],
    };

    static ARCHIVE_REFUND: AgentAuthority = AgentAuthority {
        action: "archive_refund",
        module_path: "billing::refunds",
        location: "billing/refunds.rs:70",
        grant: &REFUND_GRANT,
        effects: WRITE_ONLY,
        asserted_effect_free_sites: 0,
        asserted_effect_free: &[],
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
            operation_id: handler,
            module_path,
            mcp_tool,
            // The fixtures speak in terms of "is a tool"; how it got that way
            // is the hatch tests' business, and they set it explicitly.
            exposed_by: mcp_tool.then_some(McpExposedBy::Attribute),
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

    // ── The unaudited gate (#1691 P1-2) ──────────────────────────────

    fn manifest_with_sink(
        authorities: &[&'static AgentAuthority],
        routes: &[RouteSummary],
        sink: bool,
    ) -> AgentAuthorityManifest {
        AgentAuthorityManifest::from_parts(authorities, &[&REFUND_GRANT], routes, sink)
    }

    #[test]
    fn no_sink_plus_an_action_nothing_can_undo_fails_the_check() {
        // The one combination the runtime cannot catch: with no `AuditLogger`
        // installed the attempt write returns `Ok(())` without writing, so the
        // dispatcher's fail-closed refusal sees a success and refuses nothing.
        // Neither the attempt nor the outcome is recorded anywhere.
        let manifest =
            manifest_with_sink(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)], false);
        let failure = format_unaudited_failure(&manifest, false).expect("must fail");
        assert!(failure.contains("draft_refund"), "{failure}");
        assert!(failure.contains("RefundDrafter"), "{failure}");
        assert!(failure.contains("compensable"), "{failure}");
        // The message has to say why the runtime did not save them, or the
        // reader will assume the documented refusal already covers this.
        assert!(failure.contains("with_audit_sink"), "{failure}");
        assert!(failure.contains("trivially succeeds"), "{failure}");
    }

    #[test]
    fn a_configured_sink_clears_the_gate() {
        let manifest = manifest_with_sink(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)], true);
        assert!(format_unaudited_failure(&manifest, false).is_none());
    }

    #[test]
    fn an_ungoverned_mutating_tool_trips_the_gate_even_with_no_governed_action() {
        // Nothing declares what an ungoverned tool may do, so nothing promises
        // it is undoable either. Unrecorded, that is the same hole.
        let manifest = manifest_with_sink(
            &[],
            &[route(
                "DELETE",
                "/widgets/{id}",
                "destroy_widget",
                "shop::widgets",
                true,
                None,
            )],
            false,
        );
        let failure = format_unaudited_failure(&manifest, false).expect("must fail");
        assert!(failure.contains("destroy_widget"), "{failure}");
    }

    #[test]
    fn allow_unaudited_silences_the_gate_and_nothing_else() {
        let manifest =
            manifest_with_sink(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)], false);
        assert!(format_unaudited_failure(&manifest, true).is_none());
        // ...but the report still says the sink is missing. Allowed is not the
        // same as unseen.
        assert!(
            format_report(&manifest).contains("NOT configured"),
            "{}",
            format_report(&manifest)
        );
    }

    // ── The unregistered-authority gate (#1691 P3-1) ─────────────────

    #[test]
    fn a_route_naming_an_authority_nothing_registered_fails_the_check() {
        // The route claims an envelope, so it is not "ungoverned"; no
        // descriptor registered, so it is not an action either. It used to
        // land in no list and therefore in no gate.
        let manifest = manifest_of(&[], &[governed_route(&DRAFT_REFUND)]);
        assert!(manifest.actions.is_empty());
        assert!(manifest.ungoverned_tools.is_empty());
        let failure = format_unregistered_failure(&manifest).expect("must fail");
        assert!(failure.contains("draft_refund"), "{failure}");
        assert!(failure.contains("billing::refunds"), "{failure}");
    }

    #[test]
    fn a_registered_authority_clears_the_unregistered_gate() {
        let manifest = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        assert!(format_unregistered_failure(&manifest).is_none());
    }

    #[test]
    fn an_unregistered_authority_appearing_is_reported_as_drift() {
        let before = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        let after = manifest_of(&[], &[governed_route(&DRAFT_REFUND)]);
        let drift = format_drift(&before, &after).expect("drifted");
        assert!(drift.contains("unregistered authority"), "{drift}");
        assert!(drift.contains("draft_refund"), "{drift}");
    }

    // ── Hatch sites in the drift report (#1691 P2-5) ─────────────────

    #[test]
    fn an_added_asserted_effect_free_site_is_a_drift_line() {
        // The proved effect set does not move when a hatch is added, and
        // nothing else in the row does either -- so without this the change
        // that most needs review produced no drift at all.
        static HATCHED: AgentAuthority = AgentAuthority {
            action: "draft_refund",
            module_path: "billing::refunds",
            location: "billing/refunds.rs:28",
            grant: &REFUND_GRANT,
            effects: WRITE_ONLY,
            asserted_effect_free_sites: 1,
            asserted_effect_free: &[AssertedEffectFree {
                location: "billing/refunds.rs:41",
                reason: "the helper only formats the receipt",
            }],
        };
        let before = manifest_of(&[&DRAFT_REFUND], &[governed_route(&DRAFT_REFUND)]);
        let after = manifest_of(&[&HATCHED], &[governed_route(&HATCHED)]);
        let drift = format_drift(&before, &after).expect("drifted");
        assert!(drift.contains("+ asserted effect-free"), "{drift}");
        assert!(drift.contains("billing/refunds.rs:41"), "{drift}");
        assert!(
            drift.contains("the helper only formats the receipt"),
            "{drift}"
        );
        // And removing one is equally visible, in the other direction.
        let back = format_drift(&after, &before).expect("drifted");
        assert!(back.contains("- asserted effect-free"), "{back}");

        // The row also carries it, so a reviewer reading the committed file
        // sees the claim without running the diff.
        let row = after
            .actions
            .iter()
            .find(|r| r.action == "draft_refund")
            .expect("row");
        assert_eq!(
            row.asserted_effect_free,
            vec![AssertedEffectFreeRow {
                location: "billing/refunds.rs:41".to_string(),
                reason: "the helper only formats the receipt".to_string(),
            }]
        );
    }

    // ── Hatch-exposed tools (#1691 P2-6) ─────────────────────────────

    #[test]
    fn the_reports_say_whether_an_attribute_or_the_hatch_exposed_a_tool() {
        // A route nobody annotated, swept in by `expose_all_as_mcp()`, is
        // agent-callable exactly like an annotated one -- and is the case a
        // reviewer is least likely to know about.
        let mut hatched = route(
            "GET",
            "/reports",
            "list_reports",
            "shop::reports",
            true,
            None,
        );
        hatched.exposed_by = Some(McpExposedBy::Hatch);
        let manifest = manifest_of(&[], &[hatched]);
        let warning = format_ungoverned_warning(&manifest).expect("warned");
        assert!(warning.contains("[hatch]"), "{warning}");
        assert!(warning.contains("read-only"), "{warning}");

        let empty = manifest_of(&[], &[]);
        let drift = format_drift(&empty, &manifest).expect("drifted");
        assert!(drift.contains("[hatch]"), "{drift}");
    }

    #[test]
    fn a_renamed_tool_is_reported_by_both_names() {
        // The MCP client calls `createWidget`; the developer greps for
        // `create_widget_handler`. A report carrying only one of them makes
        // somebody do a lookup by hand.
        let mut renamed = route(
            "POST",
            "/widgets",
            "create_widget_handler",
            "shop::widgets",
            true,
            None,
        );
        renamed.operation_id = "createWidget";
        let manifest = manifest_of(&[], &[renamed]);
        let tool = manifest
            .ungoverned_tools
            .first()
            .expect("the route is an ungoverned tool");
        assert_eq!(
            describe_tool(tool),
            "POST /widgets (createWidget, handler create_widget_handler)"
        );
        // When the two agree, the report does not repeat itself.
        let plain = manifest_of(
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
        assert_eq!(
            describe_tool(&plain.ungoverned_tools[0]),
            "DELETE /widgets/{id} (destroy_widget)"
        );
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
