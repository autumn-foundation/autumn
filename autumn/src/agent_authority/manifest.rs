//! The build-time agent-authority manifest (issue #1691).
//!
//! The compiler is the gate: an agent-operable handler whose proved effects
//! fall outside its [`Grant`] does not build. This module is the *record* of
//! what those declarations add up to — one row per agent-operable action,
//! listing its envelope, the effects it was proven to have, and the grant
//! entries nothing in it uses.
//!
//! # Why it is assembled from `inventory`
//!
//! Which handlers are agent-operable is a whole-binary fact. An action declared
//! in a plugin the app merely depends on is still an action an agent can take,
//! and link-time `inventory` collection is the only place all of those
//! registrations exist together. `autumn agents manifest` therefore builds the
//! app and runs it under `AUTUMN_DUMP_AGENT_AUTHORITY=1` to read the manifest
//! back — the same shape as `autumn data-flow` (#1654), `autumn cache audit`
//! (#1716) and `autumn routes audit` (#1604).
//!
//! # Completeness over tidiness
//!
//! Two kinds of row exist so the document cannot be read as more than it is:
//! [`AgentAuthorityManifest::ungoverned_tools`] names every MCP-exposed route
//! with *no* grant (a mutating one fails `--check` unless explicitly allowed),
//! and [`AgentAuthorityManifest::excluded`] names each dimension that is
//! declared but not enforced in this slice, with the caveat spelled out in the
//! document rather than left to the guide.
//!
//! See `docs/guide/agent-authority.md`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{
    AgentAuthority, Effect, EffectKind, EffectProvenance, Grant, Reversibility, TenantScope,
};

/// Schema version of the emitted agent-authority manifest. Bumped only on
/// breaking changes to the document shape.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Machine-readable stdout marker preceding the manifest JSON emitted by the
/// `AUTUMN_DUMP_AGENT_AUTHORITY=1` dump mode.
///
/// A process-boundary protocol: `autumn agents manifest` runs the built binary
/// as a child and scans its stdout for this marker, so an app that prints
/// anything else during startup cannot corrupt the parse.
pub const AGENT_AUTHORITY_MANIFEST_MARKER: &str = "[autumn:agent-authority] ";

/// The env var selecting the app binary's agent-authority dump mode.
pub const DUMP_ENV: &str = "AUTUMN_DUMP_AGENT_AUTHORITY";

// ── Descriptors published by the macros ──────────────────────────────

/// One agent-operable action, published by `#[agent_operable]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentAuthorityDescriptor(pub &'static AgentAuthority);

inventory::collect!(AgentAuthorityDescriptor);

/// One declared envelope, published by
/// [`authority_grant!`](crate::authority_grant).
///
/// Registered separately from the action so a grant that governs *nothing* is
/// still in the document: an envelope nobody uses is either dead code or a
/// handler that lost its annotation, and both are worth seeing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantDescriptor(pub &'static Grant);

inventory::collect!(GrantDescriptor);

// ── Inputs the app supplies ──────────────────────────────────────────

/// The slice of a [`Route`](crate::Route) the manifest needs.
///
/// Built in `AppBuilder`'s dump mode from the route table and each route's
/// `ApiDoc`, so this module needs no dependency on the router or the `openapi`
/// feature.
#[derive(Debug, Clone)]
pub struct RouteSummary {
    /// HTTP method, uppercase (`"GET"`, `"POST"`).
    pub method: String,
    /// Route path template, with `{param}` placeholders.
    pub path: String,
    /// Handler function name.
    pub handler: &'static str,
    /// Handler's `module_path!()`.
    pub module_path: &'static str,
    /// Whether the route is exposed as an MCP tool.
    pub mcp_tool: bool,
    /// The handler's authority, when it carries `#[agent_operable]`.
    pub agent_authority: Option<&'static AgentAuthority>,
}

// ── The manifest document ────────────────────────────────────────────

/// Where an action is reachable from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRef {
    /// HTTP method.
    pub method: String,
    /// Route path template.
    pub path: String,
    /// Whether the route is exposed as an MCP tool.
    pub mcp_tool: bool,
}

/// The envelope an action was checked against, as it appears in a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantSummary {
    /// The grant's declared name.
    pub name: String,
    /// How reversible the action is allowed to be.
    pub reversibility: Reversibility,
    /// Whether it may leave its tenant.
    pub tenant_scope: TenantScope,
    /// Declared rate cap. Declared only — see
    /// [`AgentAuthorityManifest::excluded`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<String>,
    /// Declared spend cap. Declared only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend: Option<String>,
}

/// One effect, as it appears in a row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EffectRow {
    /// What kind of effect it is.
    pub kind: EffectKind,
    /// What it acts on.
    pub subject: String,
    /// How strong the claim is.
    pub provenance: EffectProvenance,
    /// `file:line` of the call it was proven at.
    pub location: String,
}

/// One agent-operable action and everything it may do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRow {
    /// The handler function's name.
    pub action: String,
    /// The handler's module path — the half of the join key that keeps two
    /// crates' same-named handlers apart.
    pub module_path: String,
    /// `file:line` of the handler.
    pub location: String,
    /// `"mcp-tool"`, `"http-route"`, or `"not-exposed"` when no route in this
    /// binary reaches the handler.
    pub exposure: String,
    /// `"provable"` when every effect was derived by the analyser;
    /// `"declared"` when the author wrote any of them down by hand.
    pub provenance: String,
    /// The route the action is reachable at, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteRef>,
    /// The envelope it was checked against.
    pub grant: GrantSummary,
    /// Every proved or declared effect, sorted and deduplicated.
    pub effects: Vec<EffectRow>,
    /// Grant entries no effect of this action uses, as `"<dimension>: <entry>"`
    /// — authority granted and not exercised, which is the thing to take away.
    pub unused_grant_entries: Vec<String>,
}

/// One declared envelope, whether or not anything uses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRow {
    /// The grant's declared name.
    pub name: String,
    /// `file:line` of the declaration.
    pub location: String,
    /// How reversible actions under it may be.
    pub reversibility: Reversibility,
    /// Whether actions under it may leave their tenant.
    pub tenant_scope: TenantScope,
    /// Models writable with a row-bounded write.
    pub writes: Vec<String>,
    /// Models writable with no proven row bound.
    pub unbounded_writes: Vec<String>,
    /// Allowed outbound URL prefixes and `alias:` entries.
    pub outbound: Vec<String>,
    /// Allowed webhook topics.
    pub webhooks: Vec<String>,
    /// Allowed jobs.
    pub jobs: Vec<String>,
    /// Declared rate cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<String>,
    /// Declared spend cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend: Option<String>,
    /// Whether any action in this binary is governed by it.
    pub used: bool,
}

/// An MCP-exposed route with no authority envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UngovernedTool {
    /// The tool (handler) name.
    pub tool: String,
    /// HTTP method.
    pub method: String,
    /// Route path template.
    pub path: String,
    /// Handler's module path.
    pub module_path: String,
    /// Whether the tool can change state — anything but `GET`/`HEAD`. A
    /// mutating ungoverned tool fails `autumn agents manifest --check`; a
    /// read-only one is warned about.
    pub mutating: bool,
}

/// A dimension the manifest records but this slice does not enforce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excluded {
    /// The dimension's name.
    pub dimension: String,
    /// What is and is not true about it at runtime, in one sentence.
    pub runtime_caveat: String,
}

/// Whether the app has somewhere to write agent audit events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditStatus {
    /// `true` when an `AuditLogger` with at least one sink is installed. When
    /// `false`, MCP tool invocations are traced but not recorded, which is a
    /// property of the *deployment*, not of any grant — so it belongs in the
    /// manifest rather than in a log line nobody reads.
    pub sink_configured: bool,
}

/// The whole binary's agent authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAuthorityManifest {
    /// Document shape version.
    pub schema_version: u32,
    /// How the document as a whole was derived.
    pub provenance: String,
    /// Whether agent invocations are recorded anywhere.
    pub audit: AuditStatus,
    /// One row per agent-operable action, sorted by `(module_path, action)`.
    pub actions: Vec<ActionRow>,
    /// Every declared envelope, sorted by `(name, location)`.
    pub grants: Vec<GrantRow>,
    /// Every MCP-exposed route with no envelope, sorted by `(path, method)`.
    pub ungoverned_tools: Vec<UngovernedTool>,
    /// Dimensions recorded but not enforced.
    pub excluded: Vec<Excluded>,
}

impl AgentAuthorityManifest {
    /// Join registered actions and grants against the app's route table.
    ///
    /// The action join key is `(module_path, action)`: the pair
    /// `#[agent_operable]` publishes and the pair the route macro carries into
    /// `ApiDoc`, so the two sides cannot disagree about which handler a route
    /// reaches.
    #[must_use]
    pub fn from_parts(
        authorities: &[&'static AgentAuthority],
        grants: &[&'static Grant],
        routes: &[RouteSummary],
        audit_sink_configured: bool,
    ) -> Self {
        // RED STUB (#1691): an empty document, so every manifest test below
        // fails for the reason it exists.
        let _ = (authorities, grants, routes, audit_sink_configured);
        Self {
            schema_version: 0,
            provenance: String::new(),
            audit: AuditStatus {
                sink_configured: false,
            },
            actions: Vec::new(),
            grants: Vec::new(),
            ungoverned_tools: Vec::new(),
            excluded: Vec::new(),
        }
    }

    /// The manifest as pretty JSON, ready to commit and diff.
    #[must_use]
    pub fn to_json(&self) -> String {
        // RED STUB (#1691).
        "{}".to_string()
    }

    /// The single stdout line the dump mode emits.
    #[must_use]
    pub fn to_dump_line(&self) -> String {
        // RED STUB (#1691): the marker is missing on purpose.
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// A human report: one line per action, plus what is ungoverned.
    #[must_use]
    pub fn summary(&self) -> String {
        // RED STUB (#1691).
        String::new()
    }

    /// Every MCP-exposed mutating tool with no envelope.
    ///
    /// The set `autumn agents manifest --check` fails on unless
    /// `--allow-ungoverned` is passed.
    #[must_use]
    pub fn ungoverned_mutating_tools(&self) -> Vec<&UngovernedTool> {
        // RED STUB (#1691).
        Vec::new()
    }
}

/// The dimensions this slice records but does not enforce.
///
/// A free function rather than a literal inside `from_parts` so the CLI's
/// report and the guide's honesty section can quote the same strings.
#[must_use]
pub fn excluded_dimensions() -> Vec<Excluded> {
    // RED STUB (#1691).
    Vec::new()
}

/// Assemble the manifest from everything linked into this binary.
///
/// `routes` and `audit_sink_configured` come from the running `AppBuilder`;
/// the actions and grants come from `inventory`.
#[must_use]
pub fn build(routes: &[RouteSummary], audit_sink_configured: bool) -> AgentAuthorityManifest {
    let authorities: Vec<&'static AgentAuthority> = inventory::iter::<AgentAuthorityDescriptor>
        .into_iter()
        .map(|d| d.0)
        .collect();
    let grants: Vec<&'static Grant> = inventory::iter::<GrantDescriptor>
        .into_iter()
        .map(|d| d.0)
        .collect();
    AgentAuthorityManifest::from_parts(&authorities, &grants, routes, audit_sink_configured)
}

/// Whether the process was started to dump the manifest rather than serve.
#[must_use]
pub fn is_dump_mode() -> bool {
    // RED STUB (#1691).
    false
}

/// Print the marker-prefixed manifest line the CLI parses.
pub fn print_manifest_dump(manifest: &AgentAuthorityManifest) {
    println!("{}", manifest.to_dump_line());
}

/// Recover a manifest from a child process's stdout.
///
/// Scans for [`AGENT_AUTHORITY_MANIFEST_MARKER`] so unrelated startup output
/// cannot corrupt the parse. Returns `None` when no marker line parses.
#[must_use]
pub fn parse_manifest_dump(stdout: &str) -> Option<AgentAuthorityManifest> {
    // RED STUB (#1691).
    let _ = stdout;
    None
}

/// Whether an HTTP method can change state.
#[must_use]
pub fn method_is_mutating(method: &str) -> bool {
    // RED STUB (#1691).
    let _ = method;
    false
}

/// Fold an [`Effect`] into its manifest row.
#[allow(dead_code)]
fn effect_row(effect: &Effect) -> EffectRow {
    EffectRow {
        kind: effect.kind,
        subject: effect.subject.to_string(),
        provenance: effect.provenance,
        location: effect.location.to_string(),
    }
}

/// Keep the helper reachable while `from_parts` is a stub, so the green phase
/// is a body change rather than a signature change.
#[allow(dead_code)]
fn unused_entries(grant: &Grant, effects: &[Effect]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let used = |kind: EffectKind, entry: &str| {
        effects.iter().any(|e| e.kind == kind && e.subject == entry)
    };
    for entry in grant.writes {
        if !used(EffectKind::Write, entry) {
            out.push(format!("writes: {entry}"));
        }
    }
    for entry in grant.unbounded_writes {
        if !used(EffectKind::UnboundedWrite, entry) {
            out.push(format!("unbounded_writes: {entry}"));
        }
    }
    for entry in grant.outbound {
        if !used(EffectKind::Outbound, entry) {
            out.push(format!("outbound: {entry}"));
        }
    }
    for entry in grant.webhooks {
        if !used(EffectKind::Webhook, entry) {
            out.push(format!("webhooks: {entry}"));
        }
    }
    for entry in grant.jobs {
        if !used(EffectKind::Job, entry) {
            out.push(format!("jobs: {entry}"));
        }
    }
    out
}

/// Keep `BTreeMap`, `write!` and [`effect_row`] live while the builder is a
/// stub.
#[allow(dead_code)]
fn scaffolding_is_used(effects: &[Effect]) -> String {
    let mut rows: BTreeMap<(&str, &str), Vec<EffectRow>> = BTreeMap::new();
    for effect in effects {
        rows.entry((effect.subject, effect.location))
            .or_default()
            .push(effect_row(effect));
    }
    let mut out = String::new();
    let _ = write!(out, "{}", rows.len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Fixtures ─────────────────────────────────────────────────────

    static REFUND_GRANT: Grant = Grant {
        name: "RefundDrafter",
        writes: &["Refund", "Payment"],
        unbounded_writes: &["StaleDraft"],
        tenant_scope: TenantScope::Scoped,
        outbound: &["https://api.stripe.com/v1/refunds", "alias:stripe"],
        webhooks: &["refund.drafted"],
        jobs: &["NotifyFinance", "audit_export"],
        rate: Some("10/min"),
        spend: Some("500.00 USD"),
        reversibility: Reversibility::Compensable,
        location: "billing/refunds.rs:10",
    };

    static UNUSED_GRANT: Grant = Grant {
        name: "NobodyUsesThis",
        writes: &["Ghost"],
        unbounded_writes: &[],
        tenant_scope: TenantScope::Scoped,
        outbound: &[],
        webhooks: &[],
        jobs: &[],
        rate: None,
        spend: None,
        reversibility: Reversibility::Reversible,
        location: "billing/dead.rs:1",
    };

    /// Deliberately unsorted and containing an exact duplicate: `inventory`
    /// hands descriptors back in link order and the analyser walks branches
    /// independently, so the manifest has to impose both.
    static REFUND_EFFECTS: &[Effect] = &[
        Effect {
            kind: EffectKind::Outbound,
            subject: "https://api.stripe.com/v1/refunds",
            location: "billing/refunds.rs:42",
            provenance: EffectProvenance::Syntactic,
        },
        Effect {
            kind: EffectKind::Write,
            subject: "Refund",
            location: "billing/refunds.rs:30",
            provenance: EffectProvenance::TypeResolved,
        },
        Effect {
            kind: EffectKind::Write,
            subject: "Refund",
            location: "billing/refunds.rs:30",
            provenance: EffectProvenance::TypeResolved,
        },
    ];

    static DECLARED_EFFECTS: &[Effect] = &[Effect {
        kind: EffectKind::Write,
        subject: "Refund",
        location: "billing/sweep.rs:12",
        provenance: EffectProvenance::Declared,
    }];

    static DRAFT_REFUND: AgentAuthority = AgentAuthority {
        action: "draft_refund",
        module_path: "billing::refunds",
        location: "billing/refunds.rs:28",
        grant: &REFUND_GRANT,
        effects: REFUND_EFFECTS,
        asserted_effect_free_sites: 0,
    };

    /// Registered, but no route in this binary reaches it.
    static SWEEP_DRAFTS: AgentAuthority = AgentAuthority {
        action: "sweep_drafts",
        module_path: "admin::sweep",
        location: "admin/sweep.rs:9",
        grant: &REFUND_GRANT,
        effects: DECLARED_EFFECTS,
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

    fn governed_route() -> RouteSummary {
        route(
            "POST",
            "/refunds",
            "draft_refund",
            "billing::refunds",
            true,
            Some(&DRAFT_REFUND),
        )
    }

    fn find<'a>(manifest: &'a AgentAuthorityManifest, action: &str) -> &'a ActionRow {
        manifest
            .actions
            .iter()
            .find(|row| row.action == action)
            .unwrap_or_else(|| panic!("no `{action}` row in {:?}", manifest.actions))
    }

    // ── Shape of the document ────────────────────────────────────────

    #[test]
    fn the_document_carries_its_schema_version_and_provenance() {
        let manifest =
            AgentAuthorityManifest::from_parts(&[&DRAFT_REFUND], &[&REFUND_GRANT], &[], true);
        assert_eq!(manifest.schema_version, MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest.provenance, "provable");
        let json = manifest.to_json();
        assert!(json.contains("\"schema_version\": 1"), "{json}");
        assert!(json.contains("draft_refund"), "{json}");
    }

    #[test]
    fn the_audit_sink_status_is_recorded_in_the_document() {
        // A deployment with no sink still builds and still serves tools; the
        // manifest is where that shows up, not a startup line nobody reads.
        let with_sink = AgentAuthorityManifest::from_parts(&[], &[], &[], true);
        assert!(with_sink.audit.sink_configured);
        let without = AgentAuthorityManifest::from_parts(&[], &[], &[], false);
        assert!(!without.audit.sink_configured);
        assert!(without.to_json().contains("sink_configured"));
    }

    #[test]
    fn the_dump_line_round_trips_through_the_marker() {
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND],
            &[&REFUND_GRANT],
            &[governed_route()],
            true,
        );
        let stdout = format!("booting\n{}\ndone\n", manifest.to_dump_line());
        assert!(
            stdout.contains(AGENT_AUTHORITY_MANIFEST_MARKER),
            "the dump line must carry the marker: {stdout}"
        );
        let parsed = parse_manifest_dump(&stdout).expect("manifest parses");
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn stdout_without_a_marker_parses_to_nothing() {
        assert!(parse_manifest_dump("booting\ndone\n").is_none());
        assert!(
            parse_manifest_dump(&format!("{AGENT_AUTHORITY_MANIFEST_MARKER}not json")).is_none()
        );
    }

    // ── Rows ─────────────────────────────────────────────────────────

    #[test]
    fn rows_are_sorted_by_module_path_then_action() {
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND, &SWEEP_DRAFTS],
            &[&REFUND_GRANT],
            &[governed_route()],
            true,
        );
        let keys: Vec<(String, String)> = manifest
            .actions
            .iter()
            .map(|row| (row.module_path.clone(), row.action.clone()))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "rows must be sorted by (module_path, action)");
        assert_eq!(manifest.actions[0].module_path, "admin::sweep");
    }

    #[test]
    fn effects_are_sorted_and_deduplicated() {
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND],
            &[&REFUND_GRANT],
            &[governed_route()],
            true,
        );
        let row = find(&manifest, "draft_refund");
        // The fixture registers the same write twice: link order is
        // unspecified, so an un-deduplicated list would show up as spurious
        // drift in `--check`.
        assert_eq!(row.effects.len(), 2, "{:?}", row.effects);
        let mut sorted = row.effects.clone();
        sorted.sort();
        assert_eq!(row.effects, sorted, "effects must be sorted");
        assert_eq!(row.effects[0].kind, EffectKind::Write);
        assert_eq!(row.effects[0].subject, "Refund");
        assert_eq!(row.effects[1].kind, EffectKind::Outbound);
    }

    #[test]
    fn unused_grant_entries_name_the_authority_nothing_exercises() {
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND],
            &[&REFUND_GRANT],
            &[governed_route()],
            true,
        );
        let row = find(&manifest, "draft_refund");
        let unused = &row.unused_grant_entries;
        assert!(
            unused.contains(&"writes: Payment".to_string()),
            "{unused:?}"
        );
        assert!(
            unused.contains(&"unbounded_writes: StaleDraft".to_string()),
            "{unused:?}"
        );
        assert!(
            unused.contains(&"outbound: alias:stripe".to_string()),
            "{unused:?}"
        );
        assert!(
            unused.contains(&"webhooks: refund.drafted".to_string()),
            "{unused:?}"
        );
        assert!(
            unused.contains(&"jobs: NotifyFinance".to_string()),
            "{unused:?}"
        );
        // Exercised entries are not "unused".
        assert!(
            !unused.contains(&"writes: Refund".to_string()),
            "{unused:?}"
        );
        assert!(
            !unused.contains(&"outbound: https://api.stripe.com/v1/refunds".to_string()),
            "{unused:?}"
        );
    }

    #[test]
    fn an_action_no_route_reaches_is_marked_not_exposed() {
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND, &SWEEP_DRAFTS],
            &[&REFUND_GRANT],
            &[governed_route()],
            true,
        );
        let exposed = find(&manifest, "draft_refund");
        assert_eq!(exposed.exposure, "mcp-tool");
        assert_eq!(
            exposed.route.as_ref().map(|r| r.path.as_str()),
            Some("/refunds")
        );
        assert!(exposed.route.as_ref().expect("route").mcp_tool);

        // Registered but unreachable: a warning, never a failure — a plugin
        // may register an action the host does not mount.
        let orphan = find(&manifest, "sweep_drafts");
        assert_eq!(orphan.exposure, "not-exposed");
        assert!(orphan.route.is_none());
    }

    #[test]
    fn a_governed_route_that_is_not_an_mcp_tool_is_still_an_http_route() {
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND],
            &[&REFUND_GRANT],
            &[route(
                "POST",
                "/refunds",
                "draft_refund",
                "billing::refunds",
                false,
                Some(&DRAFT_REFUND),
            )],
            true,
        );
        assert_eq!(find(&manifest, "draft_refund").exposure, "http-route");
    }

    #[test]
    fn a_hand_declared_effect_downgrades_the_row_provenance() {
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND, &SWEEP_DRAFTS],
            &[&REFUND_GRANT],
            &[governed_route()],
            true,
        );
        assert_eq!(find(&manifest, "draft_refund").provenance, "provable");
        assert_eq!(find(&manifest, "sweep_drafts").provenance, "declared");
    }

    #[test]
    fn an_asserted_effect_free_site_downgrades_the_row_provenance() {
        // `#[agent_effect(none, reason = "…")]` discharges an opaque statement
        // on a human's word. The row is no longer something the compiler
        // proved on its own, and the manifest must not claim otherwise.
        static DISCHARGED: AgentAuthority = AgentAuthority {
            action: "discharged",
            module_path: "billing::discharged",
            location: "billing/discharged.rs:3",
            grant: &REFUND_GRANT,
            effects: &[],
            asserted_effect_free_sites: 1,
        };
        let manifest =
            AgentAuthorityManifest::from_parts(&[&DISCHARGED], &[&REFUND_GRANT], &[], true);
        assert_eq!(find(&manifest, "discharged").provenance, "declared");
    }

    #[test]
    fn the_row_carries_the_declared_only_caps_verbatim() {
        let manifest =
            AgentAuthorityManifest::from_parts(&[&DRAFT_REFUND], &[&REFUND_GRANT], &[], true);
        let grant = &find(&manifest, "draft_refund").grant;
        assert_eq!(grant.name, "RefundDrafter");
        assert_eq!(grant.reversibility, Reversibility::Compensable);
        assert_eq!(grant.tenant_scope, TenantScope::Scoped);
        assert_eq!(grant.rate.as_deref(), Some("10/min"));
        assert_eq!(grant.spend.as_deref(), Some("500.00 USD"));
    }

    // ── Grants ───────────────────────────────────────────────────────

    #[test]
    fn every_declared_grant_appears_even_when_nothing_uses_it() {
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND],
            &[&REFUND_GRANT, &UNUSED_GRANT],
            &[governed_route()],
            true,
        );
        let names: Vec<&str> = manifest.grants.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"RefundDrafter"), "{names:?}");
        assert!(names.contains(&"NobodyUsesThis"), "{names:?}");
        let dead = manifest
            .grants
            .iter()
            .find(|g| g.name == "NobodyUsesThis")
            .expect("row");
        assert!(!dead.used, "an envelope nothing uses must say so");
        assert_eq!(dead.writes, vec!["Ghost".to_string()]);
        let live = manifest
            .grants
            .iter()
            .find(|g| g.name == "RefundDrafter")
            .expect("row");
        assert!(live.used);
    }

    // ── Ungoverned tools ─────────────────────────────────────────────

    #[test]
    fn an_mcp_exposed_route_with_no_envelope_is_reported_as_ungoverned() {
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND],
            &[&REFUND_GRANT],
            &[
                governed_route(),
                route(
                    "DELETE",
                    "/widgets/{id}",
                    "destroy_widget",
                    "shop::widgets",
                    true,
                    None,
                ),
                route(
                    "GET",
                    "/widgets",
                    "list_widgets",
                    "shop::widgets",
                    true,
                    None,
                ),
                // Not an MCP tool: an ordinary HTTP route is not an agent's to
                // call, so it is not this document's business.
                route(
                    "POST",
                    "/internal/sync",
                    "sync",
                    "shop::internal",
                    false,
                    None,
                ),
            ],
            true,
        );
        let names: Vec<&str> = manifest
            .ungoverned_tools
            .iter()
            .map(|t| t.tool.as_str())
            .collect();
        assert!(names.contains(&"destroy_widget"), "{names:?}");
        assert!(names.contains(&"list_widgets"), "{names:?}");
        assert!(!names.contains(&"draft_refund"), "{names:?}");
        assert!(!names.contains(&"sync"), "{names:?}");

        let destroy = manifest
            .ungoverned_tools
            .iter()
            .find(|t| t.tool == "destroy_widget")
            .expect("row");
        assert!(destroy.mutating, "DELETE changes state");
        assert_eq!(destroy.method, "DELETE");
        assert_eq!(destroy.path, "/widgets/{id}");
        assert_eq!(destroy.module_path, "shop::widgets");

        let list = manifest
            .ungoverned_tools
            .iter()
            .find(|t| t.tool == "list_widgets")
            .expect("row");
        assert!(!list.mutating, "GET is read-only: warn, never fail");

        let mutating: Vec<&str> = manifest
            .ungoverned_mutating_tools()
            .iter()
            .map(|t| t.tool.as_str())
            .collect();
        assert_eq!(mutating, ["destroy_widget"]);
    }

    #[test]
    fn only_get_and_head_are_read_only() {
        assert!(!method_is_mutating("GET"));
        assert!(!method_is_mutating("HEAD"));
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            assert!(
                method_is_mutating(method),
                "{method} must count as mutating"
            );
        }
    }

    // ── Excluded dimensions ──────────────────────────────────────────

    #[test]
    fn the_document_names_what_it_does_not_enforce() {
        let manifest = AgentAuthorityManifest::from_parts(&[], &[], &[], true);
        let dimensions: Vec<&str> = manifest
            .excluded
            .iter()
            .map(|e| e.dimension.as_str())
            .collect();
        for expected in [
            "rate",
            "spend",
            "outbound",
            "jobs",
            "cascading_deletes",
            "generated_repository_tools",
        ] {
            assert!(
                dimensions.contains(&expected),
                "`{expected}` must be named in `excluded`: {dimensions:?}"
            );
        }
        for entry in &manifest.excluded {
            assert!(
                !entry.runtime_caveat.trim().is_empty(),
                "`{}` needs a caveat a reader can act on",
                entry.dimension
            );
        }
        assert_eq!(manifest.excluded, excluded_dimensions());
    }

    // ── Determinism, through `inventory` ─────────────────────────────

    /// Registered through `inventory` exactly as `#[agent_operable]` and
    /// `authority_grant!` will: the determinism the `--check` gate rests on is
    /// a property of the *collected* document, not of a hand-built one.
    static INVENTORY_GRANT: Grant = Grant {
        name: "__AgentAuthorityManifestTestGrant",
        writes: &["__ManifestTestWidget", "__ManifestTestUnused"],
        unbounded_writes: &[],
        tenant_scope: TenantScope::Scoped,
        outbound: &[],
        webhooks: &[],
        jobs: &[],
        rate: None,
        spend: None,
        reversibility: Reversibility::Reversible,
        location: "manifest.rs:inventory",
    };

    static INVENTORY_ACTION: AgentAuthority = AgentAuthority {
        action: "__manifest_test_action",
        module_path: "__manifest_test",
        location: "manifest.rs:inventory",
        grant: &INVENTORY_GRANT,
        effects: &[Effect {
            kind: EffectKind::Write,
            subject: "__ManifestTestWidget",
            location: "manifest.rs:inventory",
            provenance: EffectProvenance::TypeResolved,
        }],
        asserted_effect_free_sites: 0,
    };

    inventory::submit! { GrantDescriptor(&INVENTORY_GRANT) }
    inventory::submit! { AgentAuthorityDescriptor(&INVENTORY_ACTION) }

    #[test]
    fn the_collected_manifest_is_byte_identical_across_builds() {
        let routes = vec![route(
            "POST",
            "/__manifest-test",
            "__manifest_test_action",
            "__manifest_test",
            true,
            Some(&INVENTORY_ACTION),
        )];
        let first = build(&routes, true);
        let second = build(&routes, true);
        assert_eq!(first, second);
        assert_eq!(first.to_json(), second.to_json());
    }

    #[test]
    fn the_inventory_registered_action_reaches_the_document() {
        let routes = vec![route(
            "POST",
            "/__manifest-test",
            "__manifest_test_action",
            "__manifest_test",
            true,
            Some(&INVENTORY_ACTION),
        )];
        let manifest = build(&routes, false);
        let row = find(&manifest, "__manifest_test_action");
        assert_eq!(row.module_path, "__manifest_test");
        assert_eq!(row.grant.name, "__AgentAuthorityManifestTestGrant");
        assert_eq!(row.effects.len(), 1);
        assert!(
            row.unused_grant_entries
                .contains(&"writes: __ManifestTestUnused".to_string()),
            "{:?}",
            row.unused_grant_entries
        );
        assert!(
            manifest
                .grants
                .iter()
                .any(|g| g.name == "__AgentAuthorityManifestTestGrant"),
            "the collected grant must appear"
        );
        assert!(!manifest.audit.sink_configured);
    }

    // ── The human report ─────────────────────────────────────────────

    #[test]
    fn the_summary_names_each_action_its_grant_and_what_is_ungoverned() {
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND],
            &[&REFUND_GRANT],
            &[
                governed_route(),
                route(
                    "DELETE",
                    "/widgets/{id}",
                    "destroy_widget",
                    "shop::widgets",
                    true,
                    None,
                ),
            ],
            true,
        );
        let summary = manifest.summary();
        assert!(summary.contains("draft_refund"), "{summary}");
        assert!(summary.contains("RefundDrafter"), "{summary}");
        assert!(summary.contains("compensable"), "{summary}");
        assert!(summary.contains("destroy_widget"), "{summary}");
        assert!(summary.contains("ungoverned"), "{summary}");
    }
}
