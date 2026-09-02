//! The build-time agent authority envelope (issue #1691).
//!
//! An MCP-exposed handler is an action an *agent* can take without a human in
//! the loop. What that action is allowed to do — which models it may write,
//! whether it may leave the tenant, which hosts it may call, which jobs it may
//! enqueue, how reversible the whole thing is — is exactly the kind of fact
//! that today lives in the reviewer's head. This module moves it into a
//! declared, compile-checked value.
//!
//! # The shape of the guarantee
//!
//! [`authority_grant!`](crate::authority_grant) declares a named `const`
//! [`Grant`] — the envelope. `#[agent_operable(grant = TheGrant)]` (in
//! `autumn-macros`) walks the handler body, derives the [`Effect`] set it can
//! prove, and emits one `const _: () = assert!(GRANT.allows_…(…))` per proved
//! effect, respanned to the offending call. A write the grant does not list is
//! therefore a build failure at the write, not a runtime surprise in
//! production.
//!
//! ```ignore
//! autumn_web::authority_grant! {
//!     /// Draft-only refund authority for the support agent.
//!     pub RefundDrafter {
//!         writes: [Refund, RefundNote],
//!         tenant_scope: scoped,
//!         outbound: ["https://api.stripe.com/v1/refunds"],
//!         jobs: [NotifyFinance],
//!         rate: "10/min",
//!         spend: "500.00 USD",
//!         reversibility: compensable,
//!     }
//! }
//!
//! #[post("/refunds")]
//! #[api_doc(mcp, summary = "Draft a refund")]
//! #[agent_operable(grant = RefundDrafter)]
//! async fn draft_refund(/* … */) -> AutumnResult<Json<Refund>> { /* … */ }
//! ```
//!
//! # What this is not
//!
//! The threat model is **drift detection, not an adversarial author** — the
//! posture `docs/guide/security-posture-manifest.md` states for the security
//! manifest and `classify` states for data flow. Two dimensions of the grant
//! are *declared, not enforced* in this slice: `rate` and `spend` are validated
//! for grammar at compile time and recorded in the manifest, but nothing meters
//! them at runtime. [`manifest::AgentAuthorityManifest::excluded`] carries that
//! caveat in the document itself rather than leaving it to the guide.
//!
//! See `docs/guide/agent-authority.md`.

pub mod manifest;

use serde::{Deserialize, Serialize};

pub use manifest::{AgentAuthorityDescriptor, GrantDescriptor};

// ── Effect vocabulary ────────────────────────────────────────────────

/// The kind of side effect a handler was proven (or declared) to have.
///
/// Ordered so the manifest's sort is stable and the *unsafe* side of a branch
/// join is the greater one: an [`UnboundedWrite`](Self::UnboundedWrite) beats a
/// [`Write`](Self::Write), a [`CrossTenant`](Self::CrossTenant) beats a scoped
/// query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EffectKind {
    /// A row-bounded write: `save`, `insert`, `delete_by_id`, …
    Write,
    /// A write with no proven row bound: `delete_all`, an unfiltered
    /// `diesel::update`, `delete_by_<x>` for an `x` that is not the id.
    UnboundedWrite,
    /// A query that leaves the current tenant or shard.
    CrossTenant,
    /// An outbound HTTP call.
    Outbound,
    /// An outbound webhook dispatch.
    Webhook,
    /// A background job enqueue.
    Job,
}

impl EffectKind {
    /// The stable manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::UnboundedWrite => "unbounded_write",
            Self::CrossTenant => "cross_tenant",
            Self::Outbound => "outbound",
            Self::Webhook => "webhook",
            Self::Job => "job",
        }
    }
}

impl std::fmt::Display for EffectKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How strong the claim behind an [`Effect`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EffectProvenance {
    /// The subject was recovered from the handle's *type* (a generated
    /// `Pg…Repository`'s `__AUTUMN_MODEL_IDENT`), so a rename cannot desync it.
    TypeResolved,
    /// The subject was recovered from the source text (a stripped type name, a
    /// literal URL, a job ident).
    Syntactic,
    /// A human wrote it down with `#[agent_effect(...)]`. Checked against the
    /// grant exactly like a proved effect, but the *claim* is theirs.
    Declared,
}

impl EffectProvenance {
    /// The stable manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TypeResolved => "type_resolved",
            Self::Syntactic => "syntactic",
            Self::Declared => "declared",
        }
    }
}

impl std::fmt::Display for EffectProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One proven (or declared) side effect of an agent-operable handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Effect {
    /// What kind of effect it is.
    pub kind: EffectKind,
    /// What it acts on: a model name, a URL, an `alias:<name>`, a
    /// `webhook:<topic>`, a job name, or the cross-tenant method's name.
    pub subject: &'static str,
    /// `file:line` of the call the effect was proven at.
    pub location: &'static str,
    /// How strong the claim is.
    pub provenance: EffectProvenance,
}

// ── Grant ────────────────────────────────────────────────────────────

/// How reversible an action is once taken.
///
/// The ordering is the *floor* ordering: an action whose effects require
/// `Compensable` cannot be declared `Reversible`, but may be declared
/// `Irreversible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Reversibility {
    /// Undoable by writing the previous rows back; nothing left the process.
    Reversible,
    /// Undoable only by a compensating action (a refund for a charge, a
    /// retraction for a webhook).
    Compensable,
    /// Not undoable at all.
    Irreversible,
}

impl Reversibility {
    /// The stable manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reversible => "reversible",
            Self::Compensable => "compensable",
            Self::Irreversible => "irreversible",
        }
    }

    /// Rank in the floor ordering, so the const checks can compare without
    /// `PartialOrd` (which is not `const`).
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Reversible => 0,
            Self::Compensable => 1,
            Self::Irreversible => 2,
        }
    }
}

impl std::fmt::Display for Reversibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether the action may leave its tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TenantScope {
    /// The action stays inside the caller's tenant. Any cross-tenant effect is
    /// a build failure.
    Scoped,
    /// The action is allowed to cross tenants.
    CrossTenant,
    /// The application is single-tenant: the dimension does not apply, and a
    /// cross-tenant effect is not a violation.
    None,
}

impl TenantScope {
    /// The stable manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scoped => "scoped",
            Self::CrossTenant => "cross_tenant",
            Self::None => "none",
        }
    }
}

impl std::fmt::Display for TenantScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A declared authority envelope: everything an agent-operable action is
/// allowed to do.
///
/// Const-constructible in every field so the macro-emitted
/// `const _: () = assert!(GRANT.allows_write("Refund"), "…")` can evaluate at
/// compile time. Declared with [`authority_grant!`](crate::authority_grant) —
/// hand-constructing one is possible and is exactly as strong a claim as any
/// other thing a human writes down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    /// The grant's declared name, used in diagnostics and the manifest.
    pub name: &'static str,
    /// Models this action may write with a row-bounded write.
    pub writes: &'static [&'static str],
    /// Models this action may write with no proven row bound. Never implied by
    /// [`writes`](Self::writes): deleting one row and deleting the table are
    /// different authorities.
    pub unbounded_writes: &'static [&'static str],
    /// Whether the action may leave its tenant.
    pub tenant_scope: TenantScope,
    /// Allowed outbound URL prefixes (matched at a path boundary) and
    /// `alias:<name>` entries for aliased clients.
    pub outbound: &'static [&'static str],
    /// Allowed webhook topics, matched exactly.
    pub webhooks: &'static [&'static str],
    /// Allowed job names or job type idents, matched exactly.
    pub jobs: &'static [&'static str],
    /// Declared rate cap, e.g. `"10/min"`. Validated for grammar at compile
    /// time; **not** enforced at runtime in this slice.
    pub rate: Option<&'static str>,
    /// Declared spend cap, e.g. `"500.00 USD"`. Validated for grammar at
    /// compile time; **not** enforced at runtime in this slice.
    pub spend: Option<&'static str>,
    /// How reversible the action is allowed to be.
    pub reversibility: Reversibility,
    /// `file:line` of the declaration.
    pub location: &'static str,
}

impl Grant {
    /// Whether a row-bounded write to `model` is inside the envelope.
    ///
    /// An unbounded-write authority subsumes a bounded one: an action allowed
    /// to delete the whole table is allowed to delete one row of it.
    #[must_use]
    pub const fn allows_write(&self, model: &str) -> bool {
        // RED STUB (#1691): deliberately wrong until the truth table is green.
        let _ = model;
        false
    }

    /// Whether an unbounded write to `model` is inside the envelope.
    ///
    /// Checked against [`unbounded_writes`](Self::unbounded_writes) **only** —
    /// listing a model under `writes` never grants the unbounded form.
    #[must_use]
    pub const fn allows_unbounded_write(&self, model: &str) -> bool {
        // RED STUB (#1691).
        let _ = model;
        true
    }

    /// Whether the action may leave its tenant.
    #[must_use]
    pub const fn allows_cross_tenant(&self) -> bool {
        // RED STUB (#1691).
        true
    }

    /// Whether `url` is covered by a declared outbound prefix.
    ///
    /// A prefix matches only when it ends at a path boundary (`/`, `?`, `#` or
    /// the end of the URL), so `https://api.example.com/v1/refunds` does not
    /// authorise `https://api.example.com/v1/refunds-evil`.
    #[must_use]
    pub const fn allows_outbound(&self, url: &str) -> bool {
        // RED STUB (#1691).
        let _ = url;
        false
    }

    /// Whether `topic` is a declared webhook topic (exact match).
    #[must_use]
    pub const fn allows_webhook(&self, topic: &str) -> bool {
        // RED STUB (#1691).
        let _ = topic;
        false
    }

    /// Whether `job` is a declared job (exact match).
    #[must_use]
    pub const fn allows_job(&self, job: &str) -> bool {
        // RED STUB (#1691).
        let _ = job;
        false
    }

    /// Whether the declared reversibility is at or above the floor the proved
    /// effects require.
    ///
    /// An `Outbound`, `Webhook`, `Job`, `CrossTenant` or `UnboundedWrite`
    /// effect floors the action at [`Reversibility::Compensable`]: none of them
    /// can be undone by writing the previous rows back.
    #[must_use]
    pub const fn allows_reversibility_floor(&self, floor: Reversibility) -> bool {
        // RED STUB (#1691).
        let _ = floor;
        true
    }
}

/// The reversibility floor one effect kind imposes.
#[must_use]
pub const fn reversibility_floor_of(kind: EffectKind) -> Reversibility {
    // RED STUB (#1691).
    let _ = kind;
    Reversibility::Reversible
}

// ── Grammar of the declared-only dimensions ──────────────────────────

/// Whether a declared rate cap is well formed: `<positive integer>/<unit>`
/// where the unit is one of `s`, `sec`, `min`, `hour`, `day`.
///
/// `const` so [`authority_grant!`](crate::authority_grant) can reject a typo at
/// compile time rather than recording it in the manifest.
#[must_use]
pub const fn rate_is_wellformed(rate: &str) -> bool {
    // RED STUB (#1691).
    let _ = rate;
    true
}

/// Whether a declared spend cap is well formed: `<decimal> <ISO 4217 code>`,
/// e.g. `"500.00 USD"`. The code is three uppercase ASCII letters; the amount
/// is a non-negative decimal.
#[must_use]
pub const fn spend_is_wellformed(spend: &str) -> bool {
    // RED STUB (#1691).
    let _ = spend;
    true
}

/// Whether a declared justification is blank (empty or all whitespace).
///
/// Delegates to the cache-coherence gate's rule rather than carrying a third
/// copy — the same reason [`crate::classify::reason_is_blank`] does.
#[doc(hidden)]
#[must_use]
pub const fn reason_is_blank(reason: &str) -> bool {
    crate::cache::coherence::reason_is_blank(reason)
}

// ── The registered action ────────────────────────────────────────────

/// One agent-operable action: the handler, its grant, and the effects the
/// analyser proved about it.
///
/// Emitted as a `static` by `#[agent_operable]` and published through
/// [`AgentAuthorityDescriptor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentAuthority {
    /// The handler function's name.
    pub action: &'static str,
    /// The handler's `module_path!()`, so two crates' same-named handlers stay
    /// distinct manifest rows.
    pub module_path: &'static str,
    /// `file:line` of the handler.
    pub location: &'static str,
    /// The envelope the handler was checked against.
    pub grant: &'static Grant,
    /// Every effect the analyser proved or the author declared, in source
    /// order.
    pub effects: &'static [Effect],
    /// How many `#[agent_effect(none, …)]` statements discharged an otherwise
    /// opaque site. A row with any of these (or any `Declared` effect) is
    /// reported with row provenance `declared` rather than `provable`.
    pub asserted_effect_free_sites: u32,
}

/// The compile-known authority of the MCP tool currently being invoked.
///
/// Inserted as a request extension by the MCP dispatcher before the tool runs,
/// so a handler can read what it is being audited under
/// (`Extension<AgentInvocation>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInvocation {
    /// The id joining this invocation's `attempt` and outcome audit events.
    pub correlation_id: String,
    /// The MCP tool name.
    pub tool: String,
    /// The grant governing the tool, or `None` when the tool is ungoverned.
    pub grant: Option<&'static str>,
    /// The grant's declared reversibility, or `None` when ungoverned.
    pub reversibility: Option<Reversibility>,
}

// ── The declaration macro ────────────────────────────────────────────

/// Map the lowercase `reversibility:` keyword onto its variant.
#[doc(hidden)]
#[macro_export]
macro_rules! __authority_grant_reversibility {
    (reversible) => {
        $crate::agent_authority::Reversibility::Reversible
    };
    (compensable) => {
        $crate::agent_authority::Reversibility::Compensable
    };
    (irreversible) => {
        $crate::agent_authority::Reversibility::Irreversible
    };
}

/// Declare a named authority envelope and register it in the agent-authority
/// manifest.
///
/// ```ignore
/// autumn_web::authority_grant! {
///     /// Draft-only refund authority for the support agent.
///     pub RefundDrafter {
///         writes: [Refund, RefundNote],
///         unbounded_writes: [],
///         tenant_scope: scoped,
///         outbound: ["https://api.stripe.com/v1/refunds", "alias:stripe"],
///         webhooks: ["refund.drafted"],
///         jobs: [NotifyFinance, "audit_export"],
///         rate: "10/min",
///         spend: "500.00 USD",
///         reversibility: compensable,
///     }
/// }
/// ```
///
/// Every key except `reversibility` is optional; the keys may appear in any
/// order.
//
// RED STUB (#1691): this arm accepts `reversibility` alone. The any-order
// tt-muncher, the const grammar assertions and the `AgentAuthorityDescriptor`
// join land in the green phase.
#[macro_export]
macro_rules! authority_grant {
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident {
            reversibility: $rev:ident $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis const $name: $crate::agent_authority::Grant = $crate::agent_authority::Grant {
            name: stringify!($name),
            writes: &[],
            unbounded_writes: &[],
            tenant_scope: $crate::agent_authority::TenantScope::Scoped,
            outbound: &[],
            webhooks: &[],
            jobs: &[],
            rate: ::core::option::Option::None,
            spend: ::core::option::Option::None,
            reversibility: $crate::__authority_grant_reversibility!($rev),
            location: concat!(file!(), ":", line!()),
        };

        $crate::reexports::inventory::submit! {
            $crate::agent_authority::GrantDescriptor(&$name)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The envelope every truth-table case starts from: nothing allowed,
    /// scoped, reversible. Each test widens exactly the dimension it is about,
    /// so a passing assertion cannot be an accident of some other list.
    const fn empty_grant() -> Grant {
        Grant {
            name: "TestGrant",
            writes: &[],
            unbounded_writes: &[],
            tenant_scope: TenantScope::Scoped,
            outbound: &[],
            webhooks: &[],
            jobs: &[],
            rate: None,
            spend: None,
            reversibility: Reversibility::Reversible,
            location: "test:0",
        }
    }

    // ── writes ───────────────────────────────────────────────────────

    #[test]
    fn a_listed_model_is_writable_and_an_unlisted_one_is_not() {
        let grant = Grant {
            writes: &["Refund", "RefundNote"],
            ..empty_grant()
        };
        assert!(grant.allows_write("Refund"));
        assert!(grant.allows_write("RefundNote"));
        assert!(!grant.allows_write("Payment"));
        // Exact, not prefix: a model whose name merely starts with a listed
        // one is a different table.
        assert!(!grant.allows_write("RefundNoteRevision"));
        assert!(!grant.allows_write("Refun"));
    }

    #[test]
    fn an_empty_write_list_allows_nothing() {
        let grant = empty_grant();
        assert!(!grant.allows_write("Refund"));
        assert!(!grant.allows_write(""));
    }

    #[test]
    fn writes_does_not_imply_unbounded_writes() {
        // Deleting one row and deleting the table are different authorities:
        // the whole point of the second list.
        let grant = Grant {
            writes: &["Refund"],
            ..empty_grant()
        };
        assert!(grant.allows_write("Refund"));
        assert!(!grant.allows_unbounded_write("Refund"));
    }

    #[test]
    fn unbounded_writes_implies_the_bounded_form() {
        // The converse direction: an action allowed to truncate the table is
        // allowed to delete one row of it, so a handler that does both needs
        // only the one entry.
        let grant = Grant {
            unbounded_writes: &["Refund"],
            ..empty_grant()
        };
        assert!(grant.allows_unbounded_write("Refund"));
        assert!(grant.allows_write("Refund"));
        assert!(!grant.allows_unbounded_write("Payment"));
    }

    // ── outbound ─────────────────────────────────────────────────────

    #[test]
    fn an_outbound_prefix_matches_only_at_a_path_boundary() {
        let grant = Grant {
            outbound: &["https://api.stripe.com/v1/refunds"],
            ..empty_grant()
        };
        // The prefix itself, and every child of it.
        assert!(grant.allows_outbound("https://api.stripe.com/v1/refunds"));
        assert!(grant.allows_outbound("https://api.stripe.com/v1/refunds/"));
        assert!(grant.allows_outbound("https://api.stripe.com/v1/refunds/re_123"));
        assert!(grant.allows_outbound("https://api.stripe.com/v1/refunds?x=1"));
        assert!(grant.allows_outbound("https://api.stripe.com/v1/refunds#frag"));
        // The whole reason the boundary rule exists: a sibling path that
        // merely starts with the allowed one is a different endpoint.
        assert!(!grant.allows_outbound("https://api.stripe.com/v1/refunds-evil"));
        assert!(!grant.allows_outbound("https://api.stripe.com/v1/refundsevil.com"));
    }

    #[test]
    fn the_outbound_scheme_and_host_must_match_exactly() {
        let grant = Grant {
            outbound: &["https://api.stripe.com/v1/refunds"],
            ..empty_grant()
        };
        assert!(!grant.allows_outbound("http://api.stripe.com/v1/refunds"));
        assert!(!grant.allows_outbound("https://api.stripe.com.evil.test/v1/refunds"));
        assert!(!grant.allows_outbound("https://api.stripe.com/v1/charges"));
    }

    #[test]
    fn an_empty_outbound_list_allows_no_url() {
        assert!(!empty_grant().allows_outbound("https://api.stripe.com/v1/refunds"));
    }

    #[test]
    fn an_alias_entry_matches_exactly() {
        // `client.named("stripe")` yields the subject `alias:stripe`; the
        // boundary rule makes that an exact match, so `alias:stripe2` is not
        // covered by `alias:stripe`.
        let grant = Grant {
            outbound: &["alias:stripe"],
            ..empty_grant()
        };
        assert!(grant.allows_outbound("alias:stripe"));
        assert!(!grant.allows_outbound("alias:stripe2"));
        assert!(!grant.allows_outbound("alias:strip"));
        assert!(!grant.allows_outbound("https://api.stripe.com/v1/refunds"));
    }

    // ── webhooks and jobs ────────────────────────────────────────────

    #[test]
    fn a_webhook_topic_matches_exactly() {
        let grant = Grant {
            webhooks: &["refund.drafted"],
            ..empty_grant()
        };
        assert!(grant.allows_webhook("refund.drafted"));
        assert!(!grant.allows_webhook("refund.drafted.v2"));
        assert!(!grant.allows_webhook("refund"));
        assert!(!empty_grant().allows_webhook("refund.drafted"));
    }

    #[test]
    fn a_job_matches_exactly() {
        let grant = Grant {
            jobs: &["NotifyFinance", "audit_export"],
            ..empty_grant()
        };
        assert!(grant.allows_job("NotifyFinance"));
        assert!(grant.allows_job("audit_export"));
        assert!(!grant.allows_job("NotifyFinanceLater"));
        assert!(!empty_grant().allows_job("NotifyFinance"));
    }

    // ── tenant scope ─────────────────────────────────────────────────

    #[test]
    fn crossing_tenants_needs_cross_tenant_or_a_single_tenant_app() {
        assert!(!empty_grant().allows_cross_tenant());
        assert!(
            Grant {
                tenant_scope: TenantScope::CrossTenant,
                ..empty_grant()
            }
            .allows_cross_tenant()
        );
        // `none` is the single-tenant application: the dimension does not
        // apply, so a cross-tenant call is not a violation of anything.
        assert!(
            Grant {
                tenant_scope: TenantScope::None,
                ..empty_grant()
            }
            .allows_cross_tenant()
        );
    }

    // ── reversibility floor ──────────────────────────────────────────

    #[test]
    fn a_reversible_grant_cannot_carry_a_compensable_floor() {
        let grant = empty_grant();
        assert!(grant.allows_reversibility_floor(Reversibility::Reversible));
        assert!(!grant.allows_reversibility_floor(Reversibility::Compensable));
        assert!(!grant.allows_reversibility_floor(Reversibility::Irreversible));
    }

    #[test]
    fn a_compensable_grant_accepts_the_compensable_floor() {
        let grant = Grant {
            reversibility: Reversibility::Compensable,
            ..empty_grant()
        };
        assert!(grant.allows_reversibility_floor(Reversibility::Reversible));
        assert!(grant.allows_reversibility_floor(Reversibility::Compensable));
        assert!(!grant.allows_reversibility_floor(Reversibility::Irreversible));
    }

    #[test]
    fn an_irreversible_grant_accepts_every_floor() {
        let grant = Grant {
            reversibility: Reversibility::Irreversible,
            ..empty_grant()
        };
        assert!(grant.allows_reversibility_floor(Reversibility::Reversible));
        assert!(grant.allows_reversibility_floor(Reversibility::Compensable));
        assert!(grant.allows_reversibility_floor(Reversibility::Irreversible));
    }

    #[test]
    fn only_a_bounded_write_leaves_an_action_reversible() {
        assert_eq!(
            reversibility_floor_of(EffectKind::Write),
            Reversibility::Reversible
        );
        for kind in [
            EffectKind::UnboundedWrite,
            EffectKind::CrossTenant,
            EffectKind::Outbound,
            EffectKind::Webhook,
            EffectKind::Job,
        ] {
            assert_eq!(
                reversibility_floor_of(kind),
                Reversibility::Compensable,
                "{kind} must floor the action at `compensable`"
            );
        }
    }

    // ── declared-only grammars ───────────────────────────────────────

    #[test]
    fn a_well_formed_rate_is_accepted() {
        for rate in ["10/min", "100/hour", "5/sec", "1/day", "60/s"] {
            assert!(rate_is_wellformed(rate), "`{rate}` must be accepted");
        }
    }

    #[test]
    fn a_malformed_rate_is_rejected() {
        // `0/min` is rejected on purpose: a cap of zero is either a typo or a
        // way to say "never", and "never" is spelled by not declaring the
        // dimension at all.
        for rate in [
            "ten per minute",
            "10/",
            "/min",
            "0/min",
            "10/fortnight",
            "10 min",
            "",
            "-1/min",
            "10/min/extra",
        ] {
            assert!(!rate_is_wellformed(rate), "`{rate}` must be rejected");
        }
    }

    #[test]
    fn a_well_formed_spend_is_accepted() {
        for spend in ["500.00 USD", "12 EUR", "0 JPY", "1000000.99 GBP"] {
            assert!(spend_is_wellformed(spend), "`{spend}` must be accepted");
        }
    }

    #[test]
    fn a_malformed_spend_is_rejected() {
        for spend in [
            "500",
            "USD 500",
            "500.00 usd",
            "-1 USD",
            "500.00 US",
            "500.00 USDD",
            "500.00",
            "  500.00 USD",
            "500.00  USD",
            "",
        ] {
            assert!(!spend_is_wellformed(spend), "`{spend}` must be rejected");
        }
    }

    // ── blank reasons ────────────────────────────────────────────────

    #[test]
    fn a_whitespace_only_reason_is_blank() {
        assert!(reason_is_blank(""));
        assert!(reason_is_blank("   "));
        assert!(!reason_is_blank("the helper does the write"));
    }
}

// RED (#1691): the grammar this module is *for*. `authority_grant!` currently
// has one arm that accepts `reversibility` alone, so the invocation below does
// not compile — it is commented out rather than deleted so the rest of the
// truth table can still run, and it is the first thing the green phase turns
// back on.
//
// #[cfg(test)]
// mod grant_syntax {
//     use super::*;
//
//     crate::authority_grant! {
//         /// Every key, deliberately NOT in the order the guide writes them:
//         /// a macro_rules arm mismatch is unacceptable UX for a declaration
//         /// this long.
//         pub(crate) ScrambledOrder {
//             reversibility: compensable,
//             spend: "500.00 USD",
//             jobs: [NotifyFinance, "audit_export"],
//             tenant_scope: cross_tenant,
//             outbound: ["https://api.stripe.com/v1/refunds", "alias:stripe"],
//             rate: "10/min",
//             webhooks: ["refund.drafted"],
//             unbounded_writes: [StaleDraft],
//             writes: [Refund, "refund_notes"],
//         }
//     }
//
//     #[test]
//     fn a_scrambled_key_order_expands_to_the_same_envelope() {
//         assert_eq!(ScrambledOrder.name, "ScrambledOrder");
//         assert!(ScrambledOrder.allows_write("Refund"));
//         assert!(ScrambledOrder.allows_write("refund_notes"));
//         assert!(ScrambledOrder.allows_unbounded_write("StaleDraft"));
//         assert!(ScrambledOrder.allows_cross_tenant());
//         assert!(ScrambledOrder.allows_outbound("https://api.stripe.com/v1/refunds/re_1"));
//         assert!(ScrambledOrder.allows_outbound("alias:stripe"));
//         assert!(ScrambledOrder.allows_webhook("refund.drafted"));
//         assert!(ScrambledOrder.allows_job("NotifyFinance"));
//         assert!(ScrambledOrder.allows_job("audit_export"));
//         assert_eq!(ScrambledOrder.rate, Some("10/min"));
//         assert_eq!(ScrambledOrder.spend, Some("500.00 USD"));
//         assert_eq!(ScrambledOrder.reversibility, Reversibility::Compensable);
//     }
// }

/// The one arm `authority_grant!` already has, kept live so the expansion
/// itself — the `const`, the `location`, the `inventory::submit!` of a
/// promoted `&Grant` — is exercised rather than assumed while the rest of the
/// grammar is red.
#[cfg(test)]
mod grant_syntax_minimal {
    use super::*;

    crate::authority_grant! {
        /// Everything optional omitted: `reversibility` alone is a legal
        /// grant, and it denies every dimension.
        pub(crate) MinimalGrant {
            reversibility: reversible,
        }
    }

    #[test]
    fn a_grant_with_only_reversibility_denies_every_dimension() {
        assert_eq!(MinimalGrant.name, "MinimalGrant");
        assert!(MinimalGrant.location.contains("agent_authority/mod.rs"));
        assert!(!MinimalGrant.allows_write("Refund"));
        assert!(!MinimalGrant.allows_cross_tenant());
        assert!(!MinimalGrant.allows_outbound("https://api.stripe.com/"));
        assert!(!MinimalGrant.allows_job("NotifyFinance"));
        assert_eq!(MinimalGrant.reversibility, Reversibility::Reversible);
    }

    #[test]
    fn the_declared_grant_reaches_the_manifest_through_inventory() {
        assert!(
            inventory::iter::<GrantDescriptor>
                .into_iter()
                .any(|d| d.0.name == "MinimalGrant"),
            "`authority_grant!` must register the envelope it declares"
        );
    }
}
