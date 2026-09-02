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
//! Several kinds of row exist so the document cannot be read as more than it
//! is. [`AgentAuthorityManifest::ungoverned_tools`] names every MCP-exposed
//! route with *no* grant — whether an author opted it in or
//! `expose_all_as_mcp` swept it up, since an agent cannot tell the difference
//! (a mutating one fails `--check` unless explicitly allowed).
//! [`AgentAuthorityManifest::unregistered_authorities`] catches the one
//! remaining way a route could belong to neither list.
//! [`AgentAuthorityManifest::excluded`] names each dimension that is declared
//! but not enforced in this slice, with the caveat spelled out in the document
//! rather than left to the guide, and says whether the dimension could ever be
//! proved at all. And [`ActionRow::asserted_effect_free`] carries every
//! `#[agent_effect(none, …)]` hatch with its reason, so widening the blast
//! radius through one is a drift line rather than a silent no-op.
//!
//! See `docs/guide/agent-authority.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{
    AgentAuthority, AssertedEffectFree, Effect, EffectKind, EffectProvenance, Grant, Reversibility,
    TenantScope,
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
    /// Handler function name, for diagnostics — the thing a developer greps
    /// for.
    pub handler: &'static str,
    /// The route's `operationId`, which is the name an MCP client calls the
    /// tool by.
    ///
    /// Usually the handler name, but `#[api_doc(operation_id = "...")]`
    /// overrides it — and then the tool an agent invokes has a name that
    /// appears nowhere in a manifest keyed on the handler.
    pub operation_id: &'static str,
    /// Handler's `module_path!()`.
    pub module_path: &'static str,
    /// Whether the route is exposed as an MCP tool: the full
    /// [`mcp_exposure`] verdict, not just the `#[api_doc(mcp)]` attribute.
    pub mcp_tool: bool,
    /// *Why* it is a tool — an explicit attribute, or the whole-API hatch.
    /// `None` when it is not a tool at all.
    ///
    /// The distinction is the point: a route nobody annotated, swept in by
    /// [`expose_all_as_mcp`](crate::app::AppBuilder::expose_all_as_mcp), is
    /// agent-callable exactly like an annotated one and is the case a reviewer
    /// is least likely to know about (#1691 P2-6).
    pub exposed_by: Option<McpExposedBy>,
    /// The handler's authority, when it carries `#[agent_operable]`.
    pub agent_authority: Option<&'static AgentAuthority>,
}

/// What made a route an MCP tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpExposedBy {
    /// An explicit `#[api_doc(mcp)]` / `Route::mcp()` opt-in on this route.
    Attribute,
    /// The whole-API hatch
    /// [`expose_all_as_mcp`](crate::app::AppBuilder::expose_all_as_mcp), which
    /// sweeps in read-only routes nobody annotated.
    Hatch,
}

impl std::fmt::Display for McpExposedBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Attribute => "attribute",
            Self::Hatch => "hatch",
        })
    }
}

/// The slice of an `ApiDoc` the MCP exposure rule reads.
///
/// Named as data so [`mcp_exposure`] can live here — unconditionally — rather
/// than behind the `mcp` feature with the projector that consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent ApiDoc flags, not a state machine
pub struct McpExposureInput<'a> {
    /// HTTP method, any case.
    pub method: &'a str,
    /// `#[api_doc(hidden)]`.
    pub hidden: bool,
    /// An explicit `mcp` opt-in.
    pub mcp_tool: bool,
    /// An explicit `mcp_exclude` opt-out.
    pub mcp_exclude: bool,
    /// `#[api_doc(mcp, stream)]` — an SSE body, so no JSON response schema.
    pub mcp_stream: bool,
    /// Whether the route declares a success response schema.
    pub has_response_schema: bool,
    /// The declared success status.
    pub success_status: u16,
    /// Whether the app called `expose_all_as_mcp()`.
    pub expose_all: bool,
}

/// Whether a route is an MCP tool, and what made it one.
///
/// A deliberate second copy of `mcp::should_expose`, which lives behind the
/// `mcp` feature while this document does not: which handlers an agent can
/// reach is not a documentation concern, and a manifest that under-reports the
/// agent surface when the `mcp` feature happens to be off would be worse than
/// no manifest. The copy is pinned equal to the original by a unit test
/// compiled under the `mcp` feature
/// (`replicated_predicate_matches_the_mcp_projector`), so the two cannot drift.
#[must_use]
pub fn mcp_exposure(input: &McpExposureInput<'_>) -> Option<McpExposedBy> {
    if input.hidden || input.mcp_exclude {
        return None;
    }
    let hatch = || {
        if input.expose_all && method_is_read_only(input.method) {
            Some(McpExposedBy::Hatch)
        } else {
            None
        }
    };
    // A streaming tool returns an SSE body, so it has no JSON response schema
    // by nature and is eligible purely on its opt-in (or the hatch).
    if input.mcp_stream {
        if input.mcp_tool {
            return Some(McpExposedBy::Attribute);
        }
        return hatch();
    }
    // JSON-out only: a response schema is the structural signal that this is a
    // JSON endpoint rather than an HTML/Maud route. A status whose body is
    // empty *by contract* (204/205) stays eligible.
    if !input.has_response_schema && !has_empty_body_contract(input.success_status) {
        return None;
    }
    if input.mcp_tool {
        return Some(McpExposedBy::Attribute);
    }
    hatch()
}

/// `GET` and `HEAD` are read-only; everything else mutates.
///
/// The inverse of [`method_is_mutating`], named separately because the hatch
/// rule reads as "read-only verbs only" and a `!` there would invite the
/// fail-closed default to be read backwards.
#[must_use]
pub fn method_is_read_only(method: &str) -> bool {
    !method_is_mutating(method)
}

/// A success status whose body is empty by contract (RFC 9110).
const fn has_empty_body_contract(status: u16) -> bool {
    matches!(status, 204 | 205)
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

/// One `#[agent_effect(none, reason = "…")]` site, as it appears in a row.
///
/// Serialised in full — location *and* reason — because a hatch whose whole
/// value is reviewability has to appear in the artefact reviewers read. Before
/// this existed, adding a hatch over a statement that really did charge a card
/// changed the proved effect set not at all and produced no drift line at all
/// (#1691 P2-5).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AssertedEffectFreeRow {
    /// `file:line` of the annotated statement.
    pub location: String,
    /// The author's justification, verbatim.
    pub reason: String,
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
    /// Every statement the author asserted effect-free, with the reason given.
    ///
    /// `#[serde(default)]` so a manifest committed before this field existed
    /// still parses: an absent list means "none recorded", which is what those
    /// documents meant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asserted_effect_free: Vec<AssertedEffectFreeRow>,
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
    /// The MCP tool name, as a client calls it (the route's `operationId`).
    pub tool: String,
    /// The handler function's name, for grepping. Equal to [`Self::tool`]
    /// unless the route overrides its `operation_id`.
    ///
    /// `#[serde(default)]` so a manifest committed before the two were told
    /// apart still loads and reports drift, rather than failing to parse.
    #[serde(default)]
    pub handler: String,
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
    /// Whether an author opted this route in or the whole-API hatch swept it
    /// up. Defaults to `attribute` when reading a manifest written before the
    /// distinction was recorded.
    #[serde(default = "default_exposed_by")]
    pub exposed_by: McpExposedBy,
}

const fn default_exposed_by() -> McpExposedBy {
    McpExposedBy::Attribute
}

/// A route pointing at an authority no `#[agent_operable]` registered.
///
/// Cannot arise from the macros — `#[agent_operable]` always emits the static
/// and its `inventory::submit!` together — but a hand-written static plus a
/// hand-written marker produces exactly this, and it would otherwise appear in
/// *neither* [`AgentAuthorityManifest::actions`] nor
/// [`AgentAuthorityManifest::ungoverned_tools`]: a tool invisible to the
/// `--check` gate. Surfaced as a loud row instead (#1691 P3-1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnregisteredAuthority {
    /// The action name the route's authority claims.
    pub action: String,
    /// The module path the route's authority claims.
    pub module_path: String,
    /// The handler name.
    pub handler: String,
    /// HTTP method.
    pub method: String,
    /// Route path template.
    pub path: String,
    /// Whether the route is MCP-exposed, and so agent-callable.
    pub mcp_tool: bool,
}

/// A dimension the manifest records but this slice does not enforce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excluded {
    /// The dimension's name.
    pub dimension: String,
    /// What is and is not true about it at runtime, in one sentence.
    pub runtime_caveat: String,
    /// The strongest provenance this dimension could ever reach, on the same
    /// vocabulary [`ActionRow::provenance`] uses: `"provable"` when a future
    /// slice could decide it at compile time, `"declared"` when the best
    /// available answer will always be the author's word, and `"runtime-only"`
    /// when it is a property of a running process that no compiler can settle.
    ///
    /// Recorded so a reader can tell "not enforced yet" from "not enforceable
    /// here" — a rate cap is not a weaker version of a proof, it is a different
    /// kind of claim.
    #[serde(default = "default_eventual_provenance")]
    pub eventual_provenance: String,
}

fn default_eventual_provenance() -> String {
    "declared".to_string()
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
    /// Routes naming an authority nothing registered, sorted by
    /// `(module_path, action)`. Empty in any binary built by the macros.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unregistered_authorities: Vec<UnregisteredAuthority>,
    /// Dimensions recorded but not enforced.
    pub excluded: Vec<Excluded>,
}

impl AgentAuthorityManifest {
    /// Join registered actions and grants against the app's route table.
    ///
    /// The action join key is `(module_path, action)`: the pair
    /// `#[agent_operable]` publishes and the pair the route macro carries into
    /// `ApiDoc`, so the two sides cannot disagree about which handler a route
    /// reaches. Everything is collected through `BTreeMap`/`BTreeSet` and every
    /// list is sorted before it lands in the document, because `inventory`
    /// hands descriptors back in link order — unspecified across builds — and
    /// `--check` would otherwise report link-order churn as drift.
    #[must_use]
    pub fn from_parts(
        authorities: &[&'static AgentAuthority],
        grants: &[&'static Grant],
        routes: &[RouteSummary],
        audit_sink_configured: bool,
    ) -> Self {
        // Which route reaches which action, keyed on the authority's own
        // identity rather than on pointer equality: a route carries the very
        // `&'static AgentAuthority` the descriptor published, and naming the
        // join in data keeps it checkable.
        let mut route_for: BTreeMap<(&str, &str), &RouteSummary> = BTreeMap::new();
        for route in routes {
            if let Some(authority) = route.agent_authority {
                route_for
                    .entry((authority.module_path, authority.action))
                    .or_insert(route);
            }
        }

        let mut actions: BTreeMap<(&str, &str), ActionRow> = BTreeMap::new();
        let mut used_grants: BTreeSet<(&str, &str)> = BTreeSet::new();
        let mut all_grants: BTreeMap<(&str, &str), &'static Grant> = BTreeMap::new();

        for authority in authorities {
            let key = (authority.module_path, authority.action);
            used_grants.insert((authority.grant.name, authority.grant.location));
            all_grants.insert(
                (authority.grant.name, authority.grant.location),
                authority.grant,
            );
            if actions.contains_key(&key) {
                // The same handler registered twice is one action, not two.
                continue;
            }
            let route = route_for.get(&key).copied();
            let mut effects: Vec<EffectRow> = authority.effects.iter().map(effect_row).collect();
            effects.sort();
            effects.dedup();
            actions.insert(
                key,
                ActionRow {
                    action: authority.action.to_string(),
                    module_path: authority.module_path.to_string(),
                    location: authority.location.to_string(),
                    exposure: exposure_of(route).to_string(),
                    provenance: row_provenance(authority).to_string(),
                    route: route.map(|route| RouteRef {
                        method: route.method.clone(),
                        path: route.path.clone(),
                        mcp_tool: route.mcp_tool,
                    }),
                    grant: GrantSummary {
                        name: authority.grant.name.to_string(),
                        reversibility: authority.grant.reversibility,
                        tenant_scope: authority.grant.tenant_scope,
                        rate: authority.grant.rate.map(ToString::to_string),
                        spend: authority.grant.spend.map(ToString::to_string),
                    },
                    effects,
                    unused_grant_entries: unused_entries(authority.grant, authority.effects),
                    asserted_effect_free: authority
                        .asserted_effect_free
                        .iter()
                        .map(asserted_effect_free_row)
                        .collect(),
                },
            );
        }

        // Declared envelopes, including the ones nothing uses: an envelope with
        // no action is either dead code or a handler that lost its annotation,
        // and dropping it would hide both.
        for grant in grants {
            all_grants.insert((grant.name, grant.location), grant);
        }
        let grants: Vec<GrantRow> = all_grants
            .values()
            .map(|grant| GrantRow {
                name: grant.name.to_string(),
                location: grant.location.to_string(),
                reversibility: grant.reversibility,
                tenant_scope: grant.tenant_scope,
                writes: to_owned(grant.writes),
                unbounded_writes: to_owned(grant.unbounded_writes),
                outbound: to_owned(grant.outbound),
                webhooks: to_owned(grant.webhooks),
                jobs: to_owned(grant.jobs),
                rate: grant.rate.map(ToString::to_string),
                spend: grant.spend.map(ToString::to_string),
                used: used_grants.contains(&(grant.name, grant.location)),
            })
            .collect();

        let ungoverned_tools = ungoverned_tools_of(routes);
        let unregistered_authorities = unregistered_authorities_of(routes, &actions);

        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            // The document as a whole is what the compiler proved; a single row
            // says for itself when a human's word was taken instead.
            provenance: "provable".to_string(),
            audit: AuditStatus {
                sink_configured: audit_sink_configured,
            },
            actions: actions.into_values().collect(),
            grants,
            ungoverned_tools,
            unregistered_authorities,
            excluded: excluded_dimensions(),
        }
    }

    /// The manifest as pretty JSON, ready to commit and diff.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// The single stdout line the dump mode emits.
    #[must_use]
    pub fn to_dump_line(&self) -> String {
        format!(
            "{AGENT_AUTHORITY_MANIFEST_MARKER}{}",
            serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
        )
    }

    /// A human report: one line per action and what it may do, then the tools
    /// nothing governs, then what this slice does not enforce.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = format!(
            "{} agent-operable action{} under {} declared grant{}; {} MCP tool{} with no envelope.\n\
             Agent audit sink: {}.",
            self.actions.len(),
            plural(self.actions.len()),
            self.grants.len(),
            plural(self.grants.len()),
            self.ungoverned_tools.len(),
            plural(self.ungoverned_tools.len()),
            if self.audit.sink_configured {
                "configured"
            } else {
                "NOT configured -- invocations are traced, not recorded"
            },
        );
        for row in &self.actions {
            write_action(&mut out, row);
        }
        if !self.ungoverned_tools.is_empty() {
            let _ = write!(out, "\n\nMCP tools with no envelope (ungoverned):");
            for tool in &self.ungoverned_tools {
                let _ = write!(
                    out,
                    "\n  {} {} ({}) [{}]{}",
                    tool.method,
                    tool.path,
                    tool.tool,
                    tool.exposed_by,
                    if tool.mutating {
                        " -- MUTATING"
                    } else {
                        " -- read-only"
                    }
                );
            }
        }
        if !self.unregistered_authorities.is_empty() {
            let _ = write!(
                out,
                "\n\nRoutes naming an authority nothing registered (in no list above):"
            );
            for row in &self.unregistered_authorities {
                let _ = write!(
                    out,
                    "\n  {} {} ({}) -> {}::{}{}",
                    row.method,
                    row.path,
                    row.handler,
                    row.module_path,
                    row.action,
                    if row.mcp_tool { " [mcp tool]" } else { "" }
                );
            }
        }
        let _ = write!(out, "\n\nRecorded, not enforced by this slice:");
        for entry in &self.excluded {
            let _ = write!(
                out,
                "\n  {} ({}): {}",
                entry.dimension, entry.eventual_provenance, entry.runtime_caveat
            );
        }
        out
    }

    /// Every MCP-exposed mutating tool with no envelope.
    ///
    /// The set `autumn agents manifest --check` fails on unless
    /// `--allow-ungoverned` is passed.
    #[must_use]
    pub fn ungoverned_mutating_tools(&self) -> Vec<&UngovernedTool> {
        self.ungoverned_tools
            .iter()
            .filter(|tool| tool.mutating)
            .collect()
    }

    /// Every **agent-reachable** action whose grant does not promise the
    /// effects are undoable.
    ///
    /// With no audit sink installed these are the invocations that leave no
    /// trace at all, which is what makes them the `--check` gate's business
    /// (#1691 P1-2).
    ///
    /// Restricted to `mcp-tool` exposure on purpose. A registered action that
    /// no route mounts, or that this binary mounts only as a plain HTTP route,
    /// is not something an agent can call — so it cannot produce the
    /// unrecorded *agent* invocation this gate exists to prevent, and failing
    /// on it would make a linked plugin's unused registration everybody's
    /// problem. HTTP requests have their own audit story; this document is
    /// about the agent surface, and the row's own `exposure` field is where it
    /// says which one it is.
    #[must_use]
    pub fn non_reversible_actions(&self) -> Vec<&ActionRow> {
        self.actions
            .iter()
            .filter(|row| {
                row.exposure == MCP_TOOL_EXPOSURE
                    && row.grant.reversibility != Reversibility::Reversible
            })
            .collect()
    }

    /// Whether an agent invocation of this binary can change something
    /// nothing can undo *and* nothing will record.
    ///
    /// The conjunction is the point. A missing sink is survivable when every
    /// agent-reachable action is reversible; a non-reversible action is
    /// survivable when it is recorded. Neither is survivable together — and
    /// the runtime cannot catch it, because with no sink installed the audit
    /// write trivially succeeds and the fail-closed refusal never fires.
    ///
    /// "Agent-reachable" is meant literally: only `mcp-tool` actions (see
    /// [`Self::non_reversible_actions`]) and ungoverned *mutating* tools
    /// count. An action nothing exposes to an agent is not this gate's
    /// business.
    #[must_use]
    pub fn unaudited_and_unrecoverable(&self) -> bool {
        !self.audit.sink_configured
            && (!self.non_reversible_actions().is_empty()
                || !self.ungoverned_mutating_tools().is_empty())
    }
}

/// One action's block of the human report: what it may do, on whose word, and
/// what its grant allows that it never uses.
fn write_action(out: &mut String, row: &ActionRow) {
    let _ = write!(
        out,
        "\n  {}::{} -> {} ({}, {}) {}",
        row.module_path,
        row.action,
        row.grant.name,
        row.grant.reversibility,
        row.grant.tenant_scope,
        match &row.route {
            Some(route) if route.mcp_tool =>
                format!("at {} {} [mcp tool]", route.method, route.path),
            Some(route) => format!("at {} {}", route.method, route.path),
            None => "-- not exposed by any route in this binary".to_string(),
        }
    );
    if row.effects.is_empty() {
        let _ = write!(out, "\n      no proven effects");
    }
    for effect in &row.effects {
        let _ = write!(
            out,
            "\n      {} {} ({})",
            effect.kind, effect.subject, effect.provenance
        );
    }
    for site in &row.asserted_effect_free {
        let _ = write!(
            out,
            "\n      asserted effect-free at {}: {}",
            site.location, site.reason
        );
    }
    if !row.unused_grant_entries.is_empty() {
        let _ = write!(
            out,
            "\n      granted but unused: {}",
            row.unused_grant_entries.join(", ")
        );
    }
}

/// Every MCP-exposed route with no authority envelope, sorted and deduplicated.
///
/// Sorting matters as much as the contents: `inventory` and the route table
/// hand rows back in link order, which is unspecified across builds, and
/// `--check` would otherwise report that churn as drift.
fn ungoverned_tools_of(routes: &[RouteSummary]) -> Vec<UngovernedTool> {
    let mut tools: Vec<UngovernedTool> = routes
        .iter()
        .filter(|route| route.mcp_tool && route.agent_authority.is_none())
        .map(|route| UngovernedTool {
            tool: route.operation_id.to_string(),
            handler: route.handler.to_string(),
            method: route.method.clone(),
            path: route.path.clone(),
            module_path: route.module_path.to_string(),
            mutating: method_is_mutating(&route.method),
            exposed_by: route.exposed_by.unwrap_or(McpExposedBy::Attribute),
        })
        .collect();
    tools.sort_by(|a, b| (&a.path, &a.method, &a.tool).cmp(&(&b.path, &b.method, &b.tool)));
    tools.dedup();
    tools
}

/// Routes naming an authority no descriptor registered.
///
/// Such a route belongs to no other list: [`AgentAuthorityManifest::actions`]
/// is built from `inventory`, and [`ungoverned_tools_of`] filters on routes
/// with *no* authority at all. Name it rather than lose it (#1691 P3-1).
fn unregistered_authorities_of(
    routes: &[RouteSummary],
    actions: &BTreeMap<(&str, &str), ActionRow>,
) -> Vec<UnregisteredAuthority> {
    let mut rows: Vec<UnregisteredAuthority> = routes
        .iter()
        .filter_map(|route| {
            let authority = route.agent_authority?;
            if actions.contains_key(&(authority.module_path, authority.action)) {
                return None;
            }
            Some(UnregisteredAuthority {
                action: authority.action.to_string(),
                module_path: authority.module_path.to_string(),
                handler: route.handler.to_string(),
                method: route.method.clone(),
                path: route.path.clone(),
                mcp_tool: route.mcp_tool,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        (&a.module_path, &a.action, &a.path, &a.method).cmp(&(
            &b.module_path,
            &b.action,
            &b.path,
            &b.method,
        ))
    });
    rows.dedup();
    rows
}

/// The dimensions this slice records but does not enforce.
///
/// A free function rather than a literal inside `from_parts` so the CLI's
/// report and the guide's honesty section quote the same strings. Every entry
/// is here because leaving it out would let the document be read as a stronger
/// claim than it is.
#[must_use]
pub fn excluded_dimensions() -> Vec<Excluded> {
    vec![
        Excluded {
            dimension: "rate".to_string(),
            eventual_provenance: "runtime-only".to_string(),
            runtime_caveat: "declared, not enforced in this slice: the cap is checked for grammar \
                             at compile time and recorded here, but nothing meters calls at \
                             runtime. How often a running process is called is not a fact a \
                             compiler can ever settle."
                .to_string(),
        },
        Excluded {
            dimension: "spend".to_string(),
            eventual_provenance: "runtime-only".to_string(),
            runtime_caveat: "declared, not enforced in this slice: the cap is checked for grammar \
                             at compile time and recorded here, but nothing meters spend at \
                             runtime. What an outbound call costs is known only once it is made."
                .to_string(),
        },
        Excluded {
            dimension: "outbound".to_string(),
            eventual_provenance: "provable".to_string(),
            runtime_caveat: "literal URL prefixes are proven at compile time; a host resolved at \
                             runtime, a named-client `alias:` entry and any `#[agent_effect]` \
                             declaration are `declared` provenance, not proven."
                .to_string(),
        },
        Excluded {
            dimension: "jobs".to_string(),
            eventual_provenance: "provable".to_string(),
            runtime_caveat: "the enqueue is proven and the job name is checked against the grant; \
                             what the job itself then does is outside this envelope and carries \
                             its own."
                .to_string(),
        },
        Excluded {
            dimension: "cascading_deletes".to_string(),
            eventual_provenance: "provable".to_string(),
            runtime_caveat: "a `dependent(...)` cascade is not folded into the write set in this \
                             slice, so deleting a parent may write child models the grant does \
                             not list."
                .to_string(),
        },
        Excluded {
            dimension: "generated_repository_tools".to_string(),
            eventual_provenance: "provable".to_string(),
            runtime_caveat: "tools generated by `#[repository(api, mcp)]`, and read-only routes \
                             swept in by `expose_all_as_mcp`, have no annotation site in this \
                             slice. They are not gated -- they surface under `ungoverned_tools`, \
                             each row naming whether an attribute or the hatch exposed it."
                .to_string(),
        },
    ]
}

/// Assemble the manifest from everything linked into this binary.
///
/// `routes` and `audit_sink_configured` come from the running `AppBuilder`;
/// the actions and grants come from `inventory`.
#[must_use]
pub fn build(routes: &[RouteSummary], audit_sink_configured: bool) -> AgentAuthorityManifest {
    let authorities: Vec<&'static AgentAuthority> = inventory::iter::<AgentAuthorityDescriptor>
        .into_iter()
        .map(|descriptor| descriptor.0)
        .collect();
    let grants: Vec<&'static Grant> = inventory::iter::<GrantDescriptor>
        .into_iter()
        .map(|descriptor| descriptor.0)
        .collect();
    AgentAuthorityManifest::from_parts(&authorities, &grants, routes, audit_sink_configured)
}

/// Whether the process was started to dump the manifest rather than serve.
#[must_use]
pub fn is_dump_mode() -> bool {
    std::env::var(DUMP_ENV).as_deref() == Ok("1")
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
    stdout
        .lines()
        .filter_map(|line| line.split_once(AGENT_AUTHORITY_MANIFEST_MARKER))
        .filter_map(|(_, json)| serde_json::from_str::<AgentAuthorityManifest>(json.trim()).ok())
        .next_back()
}

/// Whether an HTTP method can change state.
///
/// Everything but `GET` and `HEAD`. Fail-closed: a method this function has
/// never heard of counts as mutating, because the alternative is a tool that
/// slips past the `--check` gate on a spelling.
#[must_use]
pub fn method_is_mutating(method: &str) -> bool {
    !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD")
}

/// The [`ActionRow::exposure`] value meaning "an agent can call this".
///
/// Named because two things read it: [`exposure_of`], which writes it, and
/// [`AgentAuthorityManifest::non_reversible_actions`], whose whole correctness
/// rests on matching exactly what was written.
pub const MCP_TOOL_EXPOSURE: &str = "mcp-tool";

/// How an action is reachable, as the row spells it.
const fn exposure_of(route: Option<&RouteSummary>) -> &'static str {
    match route {
        Some(route) if route.mcp_tool => MCP_TOOL_EXPOSURE,
        Some(_) => "http-route",
        // Registered but unreachable: a plugin may register an action the host
        // does not mount. Reported, never a failure.
        None => "not-exposed",
    }
}

/// Whether a row is something the compiler proved on its own.
///
/// A hand-written `#[agent_effect(...)]` — including the `none` form that
/// discharges an opaque statement — is checked against the grant exactly like a
/// proved effect, but the *claim* is the author's. The row says so rather than
/// letting the document read as stronger than it is (#1691 R7).
fn row_provenance(authority: &AgentAuthority) -> &'static str {
    let declared = !authority.asserted_effect_free.is_empty()
        || authority.asserted_effect_free_sites > 0
        || authority
            .effects
            .iter()
            .any(|effect| effect.provenance == EffectProvenance::Declared);
    if declared { "declared" } else { "provable" }
}

/// Fold an [`Effect`] into its manifest row.
fn effect_row(effect: &Effect) -> EffectRow {
    EffectRow {
        kind: effect.kind,
        subject: effect.subject.to_string(),
        provenance: effect.provenance,
        location: effect.location.to_string(),
    }
}

/// Fold an [`AssertedEffectFree`] into its manifest row.
fn asserted_effect_free_row(site: &AssertedEffectFree) -> AssertedEffectFreeRow {
    AssertedEffectFreeRow {
        location: site.location.to_string(),
        reason: site.reason.to_string(),
    }
}

fn to_owned(entries: &[&str]) -> Vec<String> {
    entries.iter().map(ToString::to_string).collect()
}

const fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Grant entries no effect of the action exercises.
///
/// Authority granted and never used is the thing to take away, so the row names
/// it rather than leaving a reader to diff two lists by eye.
fn unused_entries(grant: &Grant, effects: &[Effect]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let used = |kind: EffectKind, entry: &str| {
        effects
            .iter()
            .any(|effect| effect.kind == kind && effect.subject == entry)
    };
    for entry in grant.writes {
        // An unbounded write is also a write, so a `writes` entry exercised
        // only in its unbounded form is not unused.
        if !used(EffectKind::Write, entry) && !used(EffectKind::UnboundedWrite, entry) {
            out.push(format!("writes: {entry}"));
        }
    }
    for entry in grant.unbounded_writes {
        if !used(EffectKind::UnboundedWrite, entry) {
            out.push(format!("unbounded_writes: {entry}"));
        }
    }
    for entry in grant.outbound {
        if !effects
            .iter()
            .any(|effect| effect.kind == EffectKind::Outbound && covers(entry, effect.subject))
        {
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

/// Whether an outbound grant entry is the one that authorised a call.
///
/// The same path-boundary rule [`Grant::allows_outbound`] enforces: an entry
/// that authorises `…/refunds/re_1` is exercised, not unused.
fn covers(entry: &str, url: &str) -> bool {
    url.strip_prefix(entry)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#']))
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

    /// A grant whose actions the compiler proved are undoable, for the audit
    /// gate: "no sink" is only a failure when paired with something nobody can
    /// put back.
    static REVERSIBLE_GRANT: Grant = Grant {
        name: "NoteEditor",
        writes: &["Note"],
        unbounded_writes: &[],
        tenant_scope: TenantScope::Scoped,
        outbound: &[],
        webhooks: &[],
        jobs: &[],
        rate: None,
        spend: None,
        reversibility: Reversibility::Reversible,
        location: "notes.rs:1",
    };

    static EDIT_NOTE: AgentAuthority = AgentAuthority {
        action: "edit_note",
        module_path: "notes",
        location: "notes.rs:20",
        grant: &REVERSIBLE_GRANT,
        effects: &[],
        asserted_effect_free_sites: 0,
        asserted_effect_free: &[],
    };

    static DRAFT_REFUND: AgentAuthority = AgentAuthority {
        action: "draft_refund",
        module_path: "billing::refunds",
        location: "billing/refunds.rs:28",
        grant: &REFUND_GRANT,
        effects: REFUND_EFFECTS,
        asserted_effect_free_sites: 0,
        asserted_effect_free: &[],
    };

    /// Registered, but no route in this binary reaches it.
    static SWEEP_DRAFTS: AgentAuthority = AgentAuthority {
        action: "sweep_drafts",
        module_path: "admin::sweep",
        location: "admin/sweep.rs:9",
        grant: &REFUND_GRANT,
        effects: DECLARED_EFFECTS,
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
    fn the_last_marker_line_wins() {
        // `AppBuilder::run` prints one line, but a plugin (or a test harness
        // wrapping the binary) could print an earlier one. The last is the one
        // this process produced, and picking the first would silently certify
        // somebody else's document.
        let first = AgentAuthorityManifest::from_parts(&[], &[], &[], false);
        let last = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND],
            &[&REFUND_GRANT],
            &[governed_route()],
            true,
        );
        let stdout = format!(
            "booting\n{}\nmore output\n{}\ndone\n",
            first.to_dump_line(),
            last.to_dump_line()
        );
        assert_eq!(parse_manifest_dump(&stdout).expect("parses"), last);
    }

    #[test]
    fn a_marker_in_the_middle_of_a_line_still_parses() {
        // The scan splits on the marker rather than requiring it at column
        // zero, so a log prefix (a timestamp, a `tracing` target) in front of
        // it does not lose the manifest.
        let manifest = AgentAuthorityManifest::from_parts(&[], &[], &[], true);
        let stdout = format!(
            "2026-09-02T00:00:00Z INFO app: {}\n",
            manifest.to_dump_line()
        );
        assert_eq!(parse_manifest_dump(&stdout).expect("parses"), manifest);
    }

    #[test]
    fn a_trailing_carriage_return_does_not_break_the_parse() {
        // A binary run through a pipe on Windows, or any CRLF-emitting shim,
        // leaves a `\r` that `lines()` does not strip. The parse trims it, so
        // a manifest is not silently "missing" on one platform.
        let manifest = AgentAuthorityManifest::from_parts(&[], &[], &[], true);
        let stdout = format!("{}\r\n", manifest.to_dump_line());
        assert_eq!(parse_manifest_dump(&stdout).expect("parses"), manifest);
    }

    #[test]
    fn two_routes_reaching_one_action_keep_the_first() {
        // A handler mounted at two paths is still one action. Which route the
        // row names is arbitrary, so it must at least be *stable*: the routes
        // are visited in order and the first wins, or `--check` would report
        // link-order churn as drift.
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND],
            &[&REFUND_GRANT],
            &[
                governed_route(),
                route(
                    "POST",
                    "/v2/refunds",
                    "draft_refund",
                    "billing::refunds",
                    true,
                    Some(&DRAFT_REFUND),
                ),
            ],
            true,
        );
        assert_eq!(manifest.actions.len(), 1, "one handler is one action");
        let row = find(&manifest, "draft_refund");
        assert_eq!(row.route.as_ref().expect("a route").path, "/refunds");
        // ...and the second route is not mistaken for an unregistered one.
        assert!(manifest.unregistered_authorities.is_empty());
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
            asserted_effect_free: &[AssertedEffectFree {
                location: "billing/discharged.rs:9",
                reason: "the helper only formats a string",
            }],
        };
        let manifest =
            AgentAuthorityManifest::from_parts(&[&DISCHARGED], &[&REFUND_GRANT], &[], true);
        assert_eq!(find(&manifest, "discharged").provenance, "declared");
    }

    #[test]
    fn each_asserted_effect_free_site_reaches_the_row_with_its_reason() {
        // The whole value of the hatch is that a reviewer can weigh the claim,
        // so both halves have to be in the artefact reviewers read. Before
        // this, a row already carrying one `Declared` effect could gain a
        // `#[agent_effect(none, ...)]` over a statement that really did charge
        // a card, and `--check` reported no drift at all (#1691 P2-5).
        static HATCHED: AgentAuthority = AgentAuthority {
            action: "hatched",
            module_path: "billing::hatched",
            location: "billing/hatched.rs:3",
            grant: &REFUND_GRANT,
            effects: &[],
            asserted_effect_free_sites: 2,
            asserted_effect_free: &[
                AssertedEffectFree {
                    location: "billing/hatched.rs:11",
                    reason: "pure formatting helper",
                },
                AssertedEffectFree {
                    location: "billing/hatched.rs:19",
                    reason: "the metrics counter takes no lock",
                },
            ],
        };
        let manifest = AgentAuthorityManifest::from_parts(&[&HATCHED], &[&REFUND_GRANT], &[], true);
        let row = find(&manifest, "hatched");
        assert_eq!(
            row.asserted_effect_free,
            vec![
                AssertedEffectFreeRow {
                    location: "billing/hatched.rs:11".to_string(),
                    reason: "pure formatting helper".to_string(),
                },
                AssertedEffectFreeRow {
                    location: "billing/hatched.rs:19".to_string(),
                    reason: "the metrics counter takes no lock".to_string(),
                },
            ]
        );
        // And it survives the round trip a committed manifest makes, or the
        // drift gate would never see it.
        let json = manifest.to_json();
        assert!(json.contains("pure formatting helper"), "{json}");
        let parsed: AgentAuthorityManifest = serde_json::from_str(&json).expect("round trip");
        assert_eq!(parsed, manifest);

        let summary = manifest.summary();
        assert!(summary.contains("asserted effect-free"), "{summary}");
        assert!(summary.contains("billing/hatched.rs:19"), "{summary}");
        assert!(
            summary.contains("the metrics counter takes no lock"),
            "{summary}"
        );
    }

    #[test]
    fn a_manifest_written_before_the_hatch_list_existed_still_parses() {
        // `#[serde(default)]` earns its place: a committed manifest from
        // before this field would otherwise fail `--check` with a parse error
        // rather than a drift report.
        let json = r#"{
            "schema_version": 1,
            "provenance": "provable",
            "audit": { "sink_configured": true },
            "actions": [{
                "action": "draft_refund",
                "module_path": "billing::refunds",
                "location": "billing/refunds.rs:20",
                "exposure": "not-exposed",
                "provenance": "provable",
                "grant": {
                    "name": "RefundDrafter",
                    "reversibility": "compensable",
                    "tenant_scope": "scoped"
                },
                "effects": [],
                "unused_grant_entries": []
            }],
            "grants": [],
            "ungoverned_tools": [],
            "excluded": []
        }"#;
        let parsed: AgentAuthorityManifest = serde_json::from_str(json).expect("parses");
        assert!(parsed.actions[0].asserted_effect_free.is_empty());
        assert!(parsed.unregistered_authorities.is_empty());
        // The audit posture, though, is NOT defaulted -- see the test below.
        assert!(parsed.audit.sink_configured);
    }

    #[test]
    fn a_manifest_missing_its_audit_posture_is_rejected_not_defaulted() {
        // Every field added since v1 defaults, so an older committed manifest
        // loads and shows drift rather than dying on a parse error. `audit`
        // deliberately does not: it has been required since v1, so a document
        // without it is not an older manifest, it is a malformed one -- and
        // silently reading it as `sink_configured: false` would turn "this file
        // is broken" into a confident claim about a deployment.
        let json = r#"{
            "schema_version": 1,
            "provenance": "provable",
            "actions": [],
            "grants": [],
            "ungoverned_tools": [],
            "excluded": []
        }"#;
        let error = serde_json::from_str::<AgentAuthorityManifest>(json)
            .expect_err("a manifest with no audit posture must not parse");
        assert!(error.to_string().contains("audit"), "{error}");
    }

    #[test]
    fn an_ungoverned_tool_is_named_by_its_operation_id_not_its_handler() {
        // `#[api_doc(operation_id = "...")]` renames the tool without renaming
        // the handler, and the tool name is what an MCP client calls. A row
        // keyed only on the handler would describe a tool by a name that
        // appears nowhere in the agent's view of the API.
        let mut renamed = route(
            "POST",
            "/widgets",
            "create_widget_handler",
            "shop::widgets",
            true,
            None,
        );
        renamed.operation_id = "createWidget";
        let manifest = AgentAuthorityManifest::from_parts(&[], &[], &[renamed], true);
        let tool = &manifest.ungoverned_tools[0];
        assert_eq!(tool.tool, "createWidget");
        assert_eq!(tool.handler, "create_widget_handler");
        assert!(tool.mutating);
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
        asserted_effect_free: &[],
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

    #[test]
    fn a_used_webhook_topic_is_not_reported_unused() {
        // The proved subject of a `Webhook` effect is the bare topic, exactly
        // as the grant entry spells it (#1691 S1.1). A prefixed subject would
        // never match its own entry, so every webhook a handler really fires
        // would be reported as granted-but-unused -- advice to take away
        // authority the code is actively using.
        static FIRES_WEBHOOK: AgentAuthority = AgentAuthority {
            action: "fire_webhook",
            module_path: "billing::hooks",
            location: "billing/hooks.rs:3",
            grant: &REFUND_GRANT,
            effects: &[Effect {
                kind: EffectKind::Webhook,
                subject: "refund.drafted",
                location: "billing/hooks.rs:9",
                provenance: EffectProvenance::Syntactic,
            }],
            asserted_effect_free_sites: 0,
            asserted_effect_free: &[],
        };
        let manifest =
            AgentAuthorityManifest::from_parts(&[&FIRES_WEBHOOK], &[&REFUND_GRANT], &[], true);
        let unused = &find(&manifest, "fire_webhook").unused_grant_entries;
        assert!(
            !unused.contains(&"webhooks: refund.drafted".to_string()),
            "a fired topic is not unused: {unused:?}"
        );
    }

    // ── The MCP exposure rule (#1691 P2-6) ───────────────────────────

    fn doc(method: &str) -> McpExposureInput<'_> {
        McpExposureInput {
            method,
            hidden: false,
            mcp_tool: false,
            mcp_exclude: false,
            mcp_stream: false,
            has_response_schema: true,
            success_status: 200,
            expose_all: false,
        }
    }

    #[test]
    fn the_whole_api_hatch_exposes_read_only_verbs_and_nothing_else() {
        // `expose_all_as_mcp()` sweeps in GETs nobody annotated. They are
        // agent-callable exactly like an annotated route, and reporting them
        // as "not a tool" left the read-only agent surface unenumerable.
        let hatch = |method| {
            mcp_exposure(&McpExposureInput {
                expose_all: true,
                ..doc(method)
            })
        };
        assert_eq!(hatch("GET"), Some(McpExposedBy::Hatch));
        assert_eq!(hatch("HEAD"), Some(McpExposedBy::Hatch));
        assert_eq!(hatch("POST"), None);
        assert_eq!(hatch("DELETE"), None);
        // Fail-closed on a verb nobody has heard of, exactly as
        // `method_is_mutating` does: the hatch is the one place an unrecognised
        // verb must not be waved through as "probably a read".
        assert_eq!(hatch("FROBNICATE"), None);
    }

    #[test]
    fn an_explicit_opt_in_beats_the_verb_and_an_opt_out_beats_everything() {
        assert_eq!(
            mcp_exposure(&McpExposureInput {
                mcp_tool: true,
                ..doc("POST")
            }),
            Some(McpExposedBy::Attribute)
        );
        for input in [
            McpExposureInput {
                mcp_tool: true,
                mcp_exclude: true,
                ..doc("POST")
            },
            McpExposureInput {
                mcp_tool: true,
                hidden: true,
                ..doc("POST")
            },
            McpExposureInput {
                expose_all: true,
                mcp_exclude: true,
                ..doc("GET")
            },
        ] {
            assert_eq!(mcp_exposure(&input), None, "{input:?}");
        }
    }

    #[test]
    fn an_html_route_tagged_mcp_is_not_reported_as_a_tool() {
        // The JSON-out gate. A Maud route someone tagged `#[api_doc(mcp)]`
        // never becomes a tool, so reporting it as an ungoverned one was noise
        // a reviewer would have to learn to ignore -- and a gate people learn
        // to ignore is not a gate.
        assert_eq!(
            mcp_exposure(&McpExposureInput {
                mcp_tool: true,
                has_response_schema: false,
                ..doc("GET")
            }),
            None
        );
        // ...but a body that is empty *by contract* is a real JSON endpoint.
        assert_eq!(
            mcp_exposure(&McpExposureInput {
                mcp_tool: true,
                has_response_schema: false,
                success_status: 204,
                ..doc("DELETE")
            }),
            Some(McpExposedBy::Attribute)
        );
        // ...and so is a stream, which has no schema by nature.
        assert_eq!(
            mcp_exposure(&McpExposureInput {
                mcp_tool: true,
                mcp_stream: true,
                has_response_schema: false,
                ..doc("GET")
            }),
            Some(McpExposedBy::Attribute)
        );
    }

    #[test]
    fn a_hatch_exposed_tool_reaches_ungoverned_tools_and_says_so() {
        let mut hatched = route(
            "GET",
            "/reports",
            "list_reports",
            "shop::reports",
            true,
            None,
        );
        hatched.exposed_by = Some(McpExposedBy::Hatch);
        let manifest = AgentAuthorityManifest::from_parts(&[], &[], &[hatched], true);
        let tool = &manifest.ungoverned_tools[0];
        assert_eq!(tool.tool, "list_reports");
        assert_eq!(tool.exposed_by, McpExposedBy::Hatch);
        assert!(!tool.mutating);
        let summary = manifest.summary();
        assert!(summary.contains("hatch"), "{summary}");
    }

    /// The unconditional copy of the exposure rule must stay equal to the
    /// projector that actually builds the tools, or the manifest describes an
    /// agent surface the app does not have.
    #[cfg(feature = "mcp")]
    #[test]
    fn replicated_predicate_matches_the_mcp_projector() {
        use crate::openapi::{ApiDoc, SchemaEntry, SchemaKind};

        // Any response schema will do -- the rule reads only whether one is
        // present, never what it says.
        const THING: SchemaEntry = SchemaEntry {
            name: "Thing",
            kind: SchemaKind::Ref,
            identity: None,
        };

        for method in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"] {
            for hidden in [false, true] {
                for mcp_tool in [false, true] {
                    for mcp_exclude in [false, true] {
                        for mcp_stream in [false, true] {
                            for has_response in [false, true] {
                                for status in [200u16, 204, 205] {
                                    for expose_all in [false, true] {
                                        let doc = ApiDoc {
                                            method,
                                            hidden,
                                            mcp_tool,
                                            mcp_exclude,
                                            mcp_stream,
                                            response: has_response.then_some(THING),
                                            success_status: status,
                                            ..ApiDoc::default()
                                        };
                                        let input = McpExposureInput {
                                            method,
                                            hidden,
                                            mcp_tool,
                                            mcp_exclude,
                                            mcp_stream,
                                            has_response_schema: has_response,
                                            success_status: status,
                                            expose_all,
                                        };
                                        assert_eq!(
                                            mcp_exposure(&input).is_some(),
                                            crate::mcp::should_expose(&doc, expose_all),
                                            "{input:?}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Routes naming an authority nothing registered (#1691 P3-1) ───

    #[test]
    fn a_route_whose_authority_never_registered_is_surfaced_not_lost() {
        // Cannot happen through the macros -- `#[agent_operable]` emits the
        // static and its `inventory::submit!` together -- but a hand-written
        // pair produces exactly this, and it lands in neither `actions` (built
        // from `inventory`) nor `ungoverned_tools` (which filters on *no*
        // authority). A tool in no list is a tool in no gate.
        let manifest =
            AgentAuthorityManifest::from_parts(&[], &[&REFUND_GRANT], &[governed_route()], true);
        assert!(manifest.actions.is_empty());
        assert!(manifest.ungoverned_tools.is_empty());
        assert_eq!(
            manifest.unregistered_authorities,
            vec![UnregisteredAuthority {
                action: "draft_refund".to_string(),
                module_path: "billing::refunds".to_string(),
                handler: "draft_refund".to_string(),
                method: "POST".to_string(),
                path: "/refunds".to_string(),
                mcp_tool: true,
            }]
        );
        let summary = manifest.summary();
        assert!(summary.contains("nothing registered"), "{summary}");
    }

    #[test]
    fn a_registered_authority_is_not_reported_unregistered() {
        let manifest = AgentAuthorityManifest::from_parts(
            &[&DRAFT_REFUND],
            &[&REFUND_GRANT],
            &[governed_route()],
            true,
        );
        assert!(manifest.unregistered_authorities.is_empty());
        assert_eq!(manifest.actions.len(), 1);
    }

    // ── The audit gate's inputs (#1691 P1-2) ─────────────────────────

    #[test]
    fn unaudited_and_unrecoverable_needs_both_halves() {
        // A missing sink is survivable when everything is reversible; a
        // non-reversible action is survivable when it is recorded. Only the
        // conjunction is the state nothing can catch at runtime.
        let non_reversible = |sink| {
            AgentAuthorityManifest::from_parts(
                &[&DRAFT_REFUND],
                &[&REFUND_GRANT],
                &[governed_route()],
                sink,
            )
            .unaudited_and_unrecoverable()
        };
        assert!(non_reversible(false), "compensable tool, no sink");
        assert!(!non_reversible(true), "compensable tool, sink present");

        // Reversible and unaudited is fine: nothing is lost that cannot be put
        // back.
        let reversible =
            AgentAuthorityManifest::from_parts(&[&EDIT_NOTE], &[&REVERSIBLE_GRANT], &[], false);
        assert!(!reversible.unaudited_and_unrecoverable());
        assert!(reversible.non_reversible_actions().is_empty());

        // ...until an ungoverned mutating tool joins it. Nothing declares what
        // that one may do, so nothing promises it can be undone either.
        let with_tool = AgentAuthorityManifest::from_parts(
            &[&EDIT_NOTE],
            &[&REVERSIBLE_GRANT],
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
        assert!(with_tool.unaudited_and_unrecoverable());
    }

    #[test]
    fn only_an_agent_reachable_action_trips_the_audit_gate() {
        // The gate exists because an *agent* can take an action nothing
        // records. A linked plugin can register an action this binary never
        // mounts, or mounts as a plain HTTP route -- no agent can call either,
        // so neither can produce the unrecorded agent invocation this is
        // about. Failing on them would make an unrelated app's dependency
        // everybody's `--allow-unaudited`.
        let gate = |routes: &[RouteSummary]| {
            AgentAuthorityManifest::from_parts(&[&DRAFT_REFUND], &[&REFUND_GRANT], routes, false)
        };

        // Registered by a plugin, mounted by nobody.
        let unmounted = gate(&[]);
        assert_eq!(find(&unmounted, "draft_refund").exposure, "not-exposed");
        assert!(unmounted.non_reversible_actions().is_empty());
        assert!(!unmounted.unaudited_and_unrecoverable());

        // Mounted, but only for humans over HTTP. Requests there have their
        // own audit story; this document is about the agent surface.
        let http_only = gate(&[route(
            "POST",
            "/refunds",
            "draft_refund",
            "billing::refunds",
            false,
            Some(&DRAFT_REFUND),
        )]);
        assert_eq!(find(&http_only, "draft_refund").exposure, "http-route");
        assert!(http_only.non_reversible_actions().is_empty());
        assert!(!http_only.unaudited_and_unrecoverable());

        // The very same action, exposed as a tool: now an agent can call it.
        let as_tool = gate(&[governed_route()]);
        assert_eq!(
            find(&as_tool, "draft_refund").exposure,
            MCP_TOOL_EXPOSURE,
            "the fixture must actually be a tool, or this test proves nothing"
        );
        assert_eq!(as_tool.non_reversible_actions().len(), 1);
        assert!(as_tool.unaudited_and_unrecoverable());
    }

    // ── Excluded dimensions ──────────────────────────────────────────

    #[test]
    fn every_excluded_dimension_says_whether_it_could_ever_be_proved() {
        // "Not enforced yet" and "not enforceable here" are different claims,
        // and a reader who cannot tell them apart will read a rate cap as a
        // weaker proof rather than as a different kind of statement.
        let excluded = excluded_dimensions();
        for entry in &excluded {
            assert!(
                ["provable", "declared", "runtime-only"]
                    .contains(&entry.eventual_provenance.as_str()),
                "{entry:?}"
            );
        }
        let of = |name: &str| {
            excluded
                .iter()
                .find(|e| e.dimension == name)
                .unwrap_or_else(|| panic!("no `{name}` dimension"))
                .eventual_provenance
                .clone()
        };
        assert_eq!(of("rate"), "runtime-only");
        assert_eq!(of("spend"), "runtime-only");
        assert_eq!(of("outbound"), "provable");
        assert_eq!(of("jobs"), "provable");
        assert_eq!(of("cascading_deletes"), "provable");
        assert_eq!(of("generated_repository_tools"), "provable");
    }

    #[test]
    fn the_hatch_caveat_describes_what_the_document_actually_does() {
        // It used to promise that `expose_all_as_mcp` tools "surface under
        // `ungoverned_tools`" while the summary derived tool-ness from the
        // attribute alone, so they surfaced nowhere. The text and the
        // behaviour now agree; this pins them together.
        let caveat = excluded_dimensions()
            .into_iter()
            .find(|e| e.dimension == "generated_repository_tools")
            .expect("dimension")
            .runtime_caveat;
        assert!(caveat.contains("ungoverned_tools"), "{caveat}");
        assert!(caveat.contains("expose_all_as_mcp"), "{caveat}");

        let mut hatched = route(
            "GET",
            "/reports",
            "list_reports",
            "shop::reports",
            true,
            None,
        );
        hatched.exposed_by = Some(McpExposedBy::Hatch);
        let manifest = AgentAuthorityManifest::from_parts(&[], &[], &[hatched], true);
        assert_eq!(
            manifest.ungoverned_tools.len(),
            1,
            "the caveat must be true"
        );
    }
}
