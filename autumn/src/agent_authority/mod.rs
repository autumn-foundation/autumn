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
    /// What it acts on: a model name, a URL, an `alias:<name>`, a webhook
    /// topic, a job name, or the cross-tenant method's name — in each case
    /// spelled exactly as the grant spells it, so `unused_grant_entries` can
    /// compare the two. [`EffectKind`] is what says which dimension it is.
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
    /// to delete the whole table is allowed to delete one row of it, so a
    /// handler that does both needs only the one entry.
    #[must_use]
    pub const fn allows_write(&self, model: &str) -> bool {
        list_contains(self.writes, model) || list_contains(self.unbounded_writes, model)
    }

    /// Whether an unbounded write to `model` is inside the envelope.
    ///
    /// Checked against [`unbounded_writes`](Self::unbounded_writes) **only** —
    /// listing a model under `writes` never grants the unbounded form. Deleting
    /// one row and deleting the table are different authorities, and that is
    /// the whole reason there are two lists.
    #[must_use]
    pub const fn allows_unbounded_write(&self, model: &str) -> bool {
        list_contains(self.unbounded_writes, model)
    }

    /// Whether the action may leave its tenant.
    #[must_use]
    pub const fn allows_cross_tenant(&self) -> bool {
        match self.tenant_scope {
            TenantScope::Scoped => false,
            // `none` is the single-tenant application: the dimension does not
            // apply, so a cross-tenant call is not a violation of anything.
            TenantScope::CrossTenant | TenantScope::None => true,
        }
    }

    /// Whether `url` is covered by a declared outbound prefix.
    ///
    /// A prefix matches only when it ends at a path boundary (`/`, `?`, `#` or
    /// the end of the URL), so `https://api.example.com/v1/refunds` does not
    /// authorise `https://api.example.com/v1/refunds-evil`. The scheme and host
    /// are part of the prefix and are therefore matched exactly, which is what
    /// keeps `http://` and `api.example.com.evil.test` out.
    ///
    /// The same rule gives `alias:<name>` entries exact matching for free: an
    /// alias carries no path, so the only match that ends at a boundary is the
    /// whole string.
    #[must_use]
    pub const fn allows_outbound(&self, url: &str) -> bool {
        let mut i = 0;
        while i < self.outbound.len() {
            if prefix_ends_at_boundary(url, self.outbound[i]) {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Whether `topic` is a declared webhook topic (exact match).
    #[must_use]
    pub const fn allows_webhook(&self, topic: &str) -> bool {
        list_contains(self.webhooks, topic)
    }

    /// Whether `job` is a declared job (exact match).
    #[must_use]
    pub const fn allows_job(&self, job: &str) -> bool {
        list_contains(self.jobs, job)
    }

    /// Whether the declared reversibility is at or above the floor the proved
    /// effects require.
    ///
    /// An `Outbound`, `Webhook`, `Job` or `UnboundedWrite` effect floors the
    /// action at [`Reversibility::Compensable`]: none of them can be undone by
    /// writing the previous rows back. A `Write` — and a `CrossTenant` reach,
    /// which by itself changes nothing — leaves the action `Reversible`; see
    /// [`reversibility_floor_of`].
    #[must_use]
    pub const fn allows_reversibility_floor(&self, floor: Reversibility) -> bool {
        self.reversibility.rank() >= floor.rank()
    }
}

/// The reversibility floor one effect kind imposes.
///
/// A row-bounded write is undone by writing the previous rows back, and
/// leaving the tenant is a question of *reach*, not of permanence: the
/// commonest cross-tenant effect is a raw `SELECT`, which changes nothing at
/// all. A cross-tenant write is not thereby excused — it records its own
/// `Write` or `UnboundedWrite` effect and takes that kind's floor. Every other
/// kind has already left the process (or the request) by the time anyone wants
/// it back.
#[must_use]
pub const fn reversibility_floor_of(kind: EffectKind) -> Reversibility {
    match kind {
        EffectKind::Write | EffectKind::CrossTenant => Reversibility::Reversible,
        _ => Reversibility::Compensable,
    }
}

// ── const string primitives ──────────────────────────────────────────
//
// `str` comparison and slicing are not `const`, and every check above has to
// run inside a `const _: () = assert!(…)` the macro emits at a call site. So
// each one is a byte loop. They are deliberately ASCII-only: every subject a
// grant names — a Rust type name, a URL prefix, a job name, a topic — is
// ASCII, and a byte-wise prefix test on UTF-8 can only split a multi-byte
// character when the prefix itself is not a character boundary, which the
// boundary check below already rejects.

/// Whether two strings are byte-for-byte equal.
const fn str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether `needle` appears in `list` exactly.
const fn list_contains(list: &[&str], needle: &str) -> bool {
    let mut i = 0;
    while i < list.len() {
        if str_eq(list[i], needle) {
            return true;
        }
        i += 1;
    }
    false
}

/// Whether `value` starts with `prefix` **and** the prefix ends at a path
/// boundary — the end of the string, or one of `/`, `?`, `#`.
///
/// An empty prefix matches nothing: a grant entry of `""` would otherwise
/// authorise every host on earth.
const fn prefix_ends_at_boundary(value: &str, prefix: &str) -> bool {
    let v = value.as_bytes();
    let p = prefix.as_bytes();
    if p.is_empty() || v.len() < p.len() {
        return false;
    }
    let mut i = 0;
    while i < p.len() {
        if v[i] != p[i] {
            return false;
        }
        i += 1;
    }
    if v.len() == p.len() {
        return true;
    }
    let next = v[p.len()];
    next == b'/' || next == b'?' || next == b'#'
}

/// Whether the bytes of `haystack` from `start` are exactly `needle`.
const fn tail_eq(haystack: &[u8], start: usize, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if haystack.len() - start != needle.len() {
        return false;
    }
    let mut i = 0;
    while i < needle.len() {
        if haystack[start + i] != needle[i] {
            return false;
        }
        i += 1;
    }
    true
}

// ── Grammar of the declared-only dimensions ──────────────────────────

/// Whether a declared rate cap is well formed: `<positive integer>/<unit>`
/// where the unit is one of `s`, `sec`, `min`, `hour`, `day`.
///
/// `const` so [`authority_grant!`](crate::authority_grant) can reject a typo at
/// compile time rather than recording it in the manifest. A cap of `0` is
/// rejected on purpose: it is either a typo or a way to say "never", and
/// "never" is spelled by not declaring the dimension at all.
#[must_use]
pub const fn rate_is_wellformed(rate: &str) -> bool {
    let b = rate.as_bytes();
    let mut i = 0;
    let mut nonzero = false;
    while i < b.len() && b[i] >= b'0' && b[i] <= b'9' {
        if b[i] != b'0' {
            nonzero = true;
        }
        i += 1;
    }
    if i == 0 || !nonzero {
        return false;
    }
    if i >= b.len() || b[i] != b'/' {
        return false;
    }
    let unit = i + 1;
    tail_eq(b, unit, "s")
        || tail_eq(b, unit, "sec")
        || tail_eq(b, unit, "min")
        || tail_eq(b, unit, "hour")
        || tail_eq(b, unit, "day")
}

/// Whether a declared spend cap is well formed, e.g. `"500.00 USD"`.
///
/// The shape is `<decimal> <ISO 4217 code>`: a non-negative decimal, exactly
/// one space, and three uppercase ASCII letters.
///
/// It is fixed rather than forgiving on purpose. A manifest is diffed,
/// and `"500 USD"` drifting to `"500  USD"` is noise no reviewer should have to
/// read past.
#[must_use]
pub const fn spend_is_wellformed(spend: &str) -> bool {
    let b = spend.as_bytes();
    let mut i = 0;
    let mut digits = 0;
    while i < b.len() && b[i] >= b'0' && b[i] <= b'9' {
        i += 1;
        digits += 1;
    }
    if digits == 0 {
        return false;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let mut fraction = 0;
        while i < b.len() && b[i] >= b'0' && b[i] <= b'9' {
            i += 1;
            fraction += 1;
        }
        if fraction == 0 {
            return false;
        }
    }
    if i >= b.len() || b[i] != b' ' {
        return false;
    }
    i += 1;
    if b.len() - i != 3 {
        return false;
    }
    let mut k = 0;
    while k < 3 {
        if b[i + k] < b'A' || b[i + k] > b'Z' {
            return false;
        }
        k += 1;
    }
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

/// One `#[agent_effect(none, reason = "…")]` site: a statement the author
/// asserted has no effects the analyser could not read for itself.
///
/// The hatch exists because a real handler sometimes calls through something
/// the analyser cannot see. Its whole value is that a reviewer can weigh the
/// claim, so the claim has to be somewhere a reviewer looks: both halves reach
/// the committed manifest, and adding a site is a drift line rather than a
/// silent widening of the blast radius (#1691 P2-5).
///
/// Const-constructible so `#[agent_operable]` can emit it into a `static`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AssertedEffectFree {
    /// `file:line` of the annotated statement.
    pub location: &'static str,
    /// The author's mandatory justification, verbatim.
    pub reason: &'static str,
}

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
    ///
    /// Kept alongside [`Self::asserted_effect_free`] as the cheap scalar the
    /// provenance rule reads; the slice is what a reviewer reads.
    pub asserted_effect_free_sites: u32,
    /// Every `#[agent_effect(none, …)]` site, with the reason the author gave
    /// for it, in source order.
    ///
    /// A count alone tells a reviewer that a hatch was used but not where or
    /// why, which is precisely the question the hatch raises.
    pub asserted_effect_free: &'static [AssertedEffectFree],
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

/// The half-built envelope [`authority_grant!`](crate::authority_grant)
/// assembles.
///
/// Not part of the public vocabulary: it exists so the macro can hand rustc a
/// plain struct literal with `..DEFAULT` for the keys the author omitted. That
/// is what makes the keys order-independent and optional without a
/// nine-accumulator token muncher — and it is also what gives a duplicate key
/// rustc's own "field `writes` specified more than once", pointing at the
/// offending line rather than at the macro.
///
/// `reversibility` is an `Option` here and not in [`Grant`] for one reason:
/// there is no default worth having. A grant that forgot to say how reversible
/// its action is has not declared the most important thing about it, so
/// [`GrantBuilder::build`] refuses it at const-eval rather than guessing.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct GrantBuilder {
    /// See [`Grant::writes`].
    pub writes: &'static [&'static str],
    /// See [`Grant::unbounded_writes`].
    pub unbounded_writes: &'static [&'static str],
    /// See [`Grant::tenant_scope`].
    pub tenant_scope: TenantScope,
    /// See [`Grant::outbound`].
    pub outbound: &'static [&'static str],
    /// See [`Grant::webhooks`].
    pub webhooks: &'static [&'static str],
    /// See [`Grant::jobs`].
    pub jobs: &'static [&'static str],
    /// See [`Grant::rate`].
    pub rate: Option<&'static str>,
    /// See [`Grant::spend`].
    pub spend: Option<&'static str>,
    /// See [`Grant::reversibility`]. Required; `None` fails the build.
    pub reversibility: Option<Reversibility>,
}

impl GrantBuilder {
    /// Every dimension denied, scoped to one tenant, reversibility unstated.
    ///
    /// The default has to be the *closed* envelope: a key an author forgot is a
    /// key they did not think about, and the safe reading of "did not think
    /// about it" is "not allowed".
    pub const DEFAULT: Self = Self {
        writes: &[],
        unbounded_writes: &[],
        tenant_scope: TenantScope::Scoped,
        outbound: &[],
        webhooks: &[],
        jobs: &[],
        rate: None,
        spend: None,
        reversibility: None,
    };

    /// Finish the envelope, checking everything that can only be checked once
    /// the whole declaration is in hand.
    ///
    /// # Panics
    ///
    /// At **compile time**, through const-eval: when `reversibility` was never
    /// declared, or when `rate` / `spend` do not parse.
    #[doc(hidden)]
    #[must_use]
    pub const fn build(self, name: &'static str, location: &'static str) -> Grant {
        let Some(reversibility) = self.reversibility else {
            panic!(
                "authority_grant! requires `reversibility:`. Add one of \
                 `reversibility: reversible`, `reversibility: compensable` or \
                 `reversibility: irreversible` to the grant -- how far an agent's action can be \
                 walked back is the one thing about it nobody should have to infer. \
                 See docs/guide/agent-authority.md"
            )
        };
        if let Some(rate) = self.rate {
            assert!(
                rate_is_wellformed(rate),
                "authority_grant!: `rate` must read `<count>/<unit>`, e.g. \"10/min\" -- the \
                 count is a positive integer and the unit is one of `s`, `sec`, `min`, `hour`, \
                 `day`. See docs/guide/agent-authority.md"
            );
        }
        if let Some(spend) = self.spend {
            assert!(
                spend_is_wellformed(spend),
                "authority_grant!: `spend` must read `<amount> <CURRENCY>`, e.g. \"500.00 USD\" \
                 -- a non-negative decimal, one space, and a three-letter uppercase ISO 4217 \
                 code. See docs/guide/agent-authority.md"
            );
        }
        Grant {
            name,
            writes: self.writes,
            unbounded_writes: self.unbounded_writes,
            tenant_scope: self.tenant_scope,
            outbound: self.outbound,
            webhooks: self.webhooks,
            jobs: self.jobs,
            rate: self.rate,
            spend: self.spend,
            reversibility,
            location,
        }
    }
}

/// One `writes` / `unbounded_writes` / `jobs` entry: an ident is stringified, a
/// string literal is taken as written.
#[doc(hidden)]
#[macro_export]
macro_rules! __authority_grant_subject {
    ($subject:literal) => {
        $subject
    };
    ($subject:ident) => {
        stringify!($subject)
    };
}

/// Map the lowercase `tenant_scope:` keyword onto its variant.
#[doc(hidden)]
#[macro_export]
macro_rules! __authority_grant_tenant_scope {
    (scoped) => {
        $crate::agent_authority::TenantScope::Scoped
    };
    (cross_tenant) => {
        $crate::agent_authority::TenantScope::CrossTenant
    };
    (none) => {
        $crate::agent_authority::TenantScope::None
    };
    ($other:tt) => {
        ::core::compile_error!(::core::concat!(
            "authority_grant!: `tenant_scope` must be `scoped`, `cross_tenant` or `none`, not `",
            ::core::stringify!($other),
            "`. `none` means the application is single-tenant and the dimension does not apply. \
             See docs/guide/agent-authority.md"
        ))
    };
}

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
    // The near-miss is common enough to be worth its own line: "compensatable"
    // is what most people reach for first.
    (compensatable) => {
        ::core::compile_error!(
            "authority_grant!: `reversibility` is spelled `compensable`, not `compensatable`. \
             See docs/guide/agent-authority.md"
        )
    };
    ($other:tt) => {
        ::core::compile_error!(::core::concat!(
            "authority_grant!: `reversibility` must be `reversible`, `compensable` or \
             `irreversible`, not `",
            ::core::stringify!($other),
            "`. See docs/guide/agent-authority.md"
        ))
    };
}

/// The body parser behind [`authority_grant!`](crate::authority_grant).
///
/// One arm per key, each rewriting `key: <declared form>` into the struct-field
/// initialiser the terminal arm splats over
/// [`GrantBuilder::DEFAULT`](crate::agent_authority::GrantBuilder::DEFAULT).
/// Because every key becomes a field of one struct literal, the keys are
/// order-independent and individually optional for free, and a repeated key is
/// rustc's own duplicate-field error at the line that wrote it.
#[doc(hidden)]
#[macro_export]
macro_rules! __authority_grant_parse {
    // ── done: emit ───────────────────────────────────────────────────
    (
        @meta [$($meta:tt)*] @vis [$($vis:tt)*] @name [$name:ident]
        @fields [$($fields:tt)*] @rest []
    ) => {
        $($meta)*
        // A grant is named for the *role* it grants, not for a magic number:
        // `RefundDrafter` reads correctly at the `#[agent_operable(grant = ...)]`
        // use site in a way `REFUND_DRAFTER` does not, so the declaration style
        // the guide teaches must not cost every author a lint.
        #[allow(non_upper_case_globals)]
        $($vis)* const $name: $crate::agent_authority::Grant = {
            // The `..DEFAULT` splat is what makes every key optional and
            // order-independent, so it is still correct — and inert — for a
            // grant that happens to declare all of them. Bound in a block
            // because an author who fills in the whole envelope should not be
            // told their own declaration is redundant.
            #[allow(clippy::needless_update)]
            let builder = $crate::agent_authority::GrantBuilder {
                $($fields)*
                ..$crate::agent_authority::GrantBuilder::DEFAULT
            };
            builder.build(
                ::core::stringify!($name),
                ::core::concat!(::core::file!(), ":", ::core::line!()),
            )
        };

        $crate::reexports::inventory::submit! {
            $crate::agent_authority::GrantDescriptor(&$name)
        }
    };

    // ── subject lists: idents or string literals ─────────────────────
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields [$($fields:tt)*]
        @rest [writes: [$($subject:tt),* $(,)?] $(, $($rest:tt)*)?]
    ) => {
        $crate::__authority_grant_parse! {
            @meta $meta @vis $vis @name $name
            @fields [$($fields)* writes: &[$($crate::__authority_grant_subject!($subject)),*],]
            @rest [$($($rest)*)?]
        }
    };
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields [$($fields:tt)*]
        @rest [unbounded_writes: [$($subject:tt),* $(,)?] $(, $($rest:tt)*)?]
    ) => {
        $crate::__authority_grant_parse! {
            @meta $meta @vis $vis @name $name
            @fields [
                $($fields)*
                unbounded_writes: &[$($crate::__authority_grant_subject!($subject)),*],
            ]
            @rest [$($($rest)*)?]
        }
    };
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields [$($fields:tt)*]
        @rest [jobs: [$($subject:tt),* $(,)?] $(, $($rest:tt)*)?]
    ) => {
        $crate::__authority_grant_parse! {
            @meta $meta @vis $vis @name $name
            @fields [$($fields)* jobs: &[$($crate::__authority_grant_subject!($subject)),*],]
            @rest [$($($rest)*)?]
        }
    };

    // ── literal-only lists ───────────────────────────────────────────
    //
    // A URL prefix or a webhook topic is data, never a Rust name, so an ident
    // here is a mistake worth naming rather than silently stringifying.
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields [$($fields:tt)*]
        @rest [outbound: [$($url:literal),* $(,)?] $(, $($rest:tt)*)?]
    ) => {
        $crate::__authority_grant_parse! {
            @meta $meta @vis $vis @name $name
            @fields [$($fields)* outbound: &[$($url),*],]
            @rest [$($($rest)*)?]
        }
    };
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields $fields:tt
        @rest [outbound: $bad:tt $(, $($rest:tt)*)?]
    ) => {
        ::core::compile_error!(
            "authority_grant!: `outbound` takes a list of string literals, e.g. \
             `outbound: [\"https://api.stripe.com/v1/refunds\", \"alias:stripe\"]` -- a URL \
             prefix matched at a path boundary, or `alias:<name>` for a named client. \
             See docs/guide/agent-authority.md"
        );
    };
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields [$($fields:tt)*]
        @rest [webhooks: [$($topic:literal),* $(,)?] $(, $($rest:tt)*)?]
    ) => {
        $crate::__authority_grant_parse! {
            @meta $meta @vis $vis @name $name
            @fields [$($fields)* webhooks: &[$($topic),*],]
            @rest [$($($rest)*)?]
        }
    };
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields $fields:tt
        @rest [webhooks: $bad:tt $(, $($rest:tt)*)?]
    ) => {
        ::core::compile_error!(
            "authority_grant!: `webhooks` takes a list of string literals, e.g. \
             `webhooks: [\"refund.drafted\"]`. See docs/guide/agent-authority.md"
        );
    };

    // ── keyword scalars ──────────────────────────────────────────────
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields [$($fields:tt)*]
        @rest [tenant_scope: $scope:tt $(, $($rest:tt)*)?]
    ) => {
        $crate::__authority_grant_parse! {
            @meta $meta @vis $vis @name $name
            @fields [$($fields)* tenant_scope: $crate::__authority_grant_tenant_scope!($scope),]
            @rest [$($($rest)*)?]
        }
    };
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields [$($fields:tt)*]
        @rest [reversibility: $reversibility:tt $(, $($rest:tt)*)?]
    ) => {
        $crate::__authority_grant_parse! {
            @meta $meta @vis $vis @name $name
            @fields [
                $($fields)*
                reversibility: ::core::option::Option::Some(
                    $crate::__authority_grant_reversibility!($reversibility)
                ),
            ]
            @rest [$($($rest)*)?]
        }
    };

    // ── declared-only caps ───────────────────────────────────────────
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields [$($fields:tt)*]
        @rest [rate: $rate:literal $(, $($rest:tt)*)?]
    ) => {
        $crate::__authority_grant_parse! {
            @meta $meta @vis $vis @name $name
            @fields [$($fields)* rate: ::core::option::Option::Some($rate),]
            @rest [$($($rest)*)?]
        }
    };
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields $fields:tt
        @rest [rate: $bad:tt $(, $($rest:tt)*)?]
    ) => {
        ::core::compile_error!(
            "authority_grant!: `rate` takes a string literal, e.g. `rate: \"10/min\"`. \
             See docs/guide/agent-authority.md"
        );
    };
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields [$($fields:tt)*]
        @rest [spend: $spend:literal $(, $($rest:tt)*)?]
    ) => {
        $crate::__authority_grant_parse! {
            @meta $meta @vis $vis @name $name
            @fields [$($fields)* spend: ::core::option::Option::Some($spend),]
            @rest [$($($rest)*)?]
        }
    };
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields $fields:tt
        @rest [spend: $bad:tt $(, $($rest:tt)*)?]
    ) => {
        ::core::compile_error!(
            "authority_grant!: `spend` takes a string literal, e.g. `spend: \"500.00 USD\"`. \
             See docs/guide/agent-authority.md"
        );
    };

    // ── anything else ────────────────────────────────────────────────
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields $fields:tt
        @rest [$key:ident : $($rest:tt)*]
    ) => {
        ::core::compile_error!(::core::concat!(
            "authority_grant!: unknown key `",
            ::core::stringify!($key),
            "`. The grammar is `writes: [..]`, `unbounded_writes: [..]`, \
             `tenant_scope: scoped | cross_tenant | none`, `outbound: [\"..\"]`, \
             `webhooks: [\"..\"]`, `jobs: [..]`, `rate: \"10/min\"`, `spend: \"500.00 USD\"`, \
             `reversibility: reversible | compensable | irreversible` (required). The keys may \
             appear in any order and all but `reversibility` may be omitted. \
             See docs/guide/agent-authority.md"
        ));
    };
    (
        @meta $meta:tt @vis $vis:tt @name $name:tt @fields $fields:tt
        @rest [$($junk:tt)*]
    ) => {
        ::core::compile_error!(
            "authority_grant!: could not parse the grant body. Each entry reads `key: value,`. \
             See docs/guide/agent-authority.md"
        );
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
/// The keys may appear in **any order** and every one but `reversibility` may
/// be omitted — an omitted key denies its dimension. `writes`,
/// `unbounded_writes` and `jobs` take idents (stringified) or string literals;
/// `outbound` and `webhooks` take string literals only. `rate` and `spend` are
/// checked for grammar at compile time and recorded in the manifest, but
/// nothing meters them at runtime in this slice — see
/// [`manifest::AgentAuthorityManifest::excluded`].
///
/// An unknown key, a missing `reversibility`, a misspelled keyword and an
/// unparsable `rate`/`spend` are all build failures. A *repeated* key is
/// rustc's own duplicate-field error, pointing at the second one.
#[macro_export]
macro_rules! authority_grant {
    // Visibility is matched as tokens rather than captured as a `vis` fragment:
    // an opaque `vis` non-terminal cannot be carried through a token muncher,
    // and three arms here is cheaper than the alternative everywhere else.
    (
        $(#[$meta:meta])*
        pub($($restricted:tt)*) $name:ident { $($body:tt)* }
    ) => {
        $crate::__authority_grant_parse! {
            @meta [$(#[$meta])*] @vis [pub($($restricted)*)] @name [$name]
            @fields [] @rest [$($body)*]
        }
    };
    (
        $(#[$meta:meta])*
        pub $name:ident { $($body:tt)* }
    ) => {
        $crate::__authority_grant_parse! {
            @meta [$(#[$meta])*] @vis [pub] @name [$name]
            @fields [] @rest [$($body)*]
        }
    };
    (
        $(#[$meta:meta])*
        $name:ident { $($body:tt)* }
    ) => {
        $crate::__authority_grant_parse! {
            @meta [$(#[$meta])*] @vis [] @name [$name]
            @fields [] @rest [$($body)*]
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
    fn a_bounded_write_and_a_cross_tenant_reach_leave_an_action_reversible() {
        // A cross-tenant *read* changes nothing, and a cross-tenant write
        // carries its own `Write`/`UnboundedWrite` row to floor the action.
        for kind in [EffectKind::Write, EffectKind::CrossTenant] {
            assert_eq!(
                reversibility_floor_of(kind),
                Reversibility::Reversible,
                "{kind} must leave the action `reversible`"
            );
        }
        for kind in [
            EffectKind::UnboundedWrite,
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

/// The declaration grammar itself: the keys are order-independent, all but
/// `reversibility` are optional, and the expansion registers the envelope.
#[cfg(test)]
// The `pub(crate)` grants below are the point: the macro matches visibility as
// tokens, and each of the three shapes it accepts needs an expansion here.
#[allow(clippy::redundant_pub_crate)]
mod grant_syntax {
    use super::*;

    crate::authority_grant! {
        /// Every key, deliberately NOT in the order the guide writes them: a
        /// `macro_rules` arm mismatch is unacceptable UX for a declaration this
        /// long, so the parser accumulates key/value pairs instead of
        /// demanding a shape.
        pub(crate) ScrambledOrder {
            reversibility: compensable,
            spend: "500.00 USD",
            jobs: [NotifyFinance, "audit_export"],
            tenant_scope: cross_tenant,
            outbound: ["https://api.stripe.com/v1/refunds", "alias:stripe"],
            rate: "10/min",
            webhooks: ["refund.drafted"],
            unbounded_writes: [StaleDraft],
            writes: [Refund, "refund_notes"],
        }
    }

    crate::authority_grant! {
        /// Everything optional omitted: `reversibility` alone is a legal grant,
        /// and it denies every dimension.
        pub(crate) MinimalGrant {
            reversibility: reversible,
        }
    }

    crate::authority_grant! {
        /// No visibility, an empty list, and no trailing comma — three shapes
        /// the parser has to accept as readily as the canonical one.
        SingleTenantSweeper {
            writes: [],
            tenant_scope: none,
            reversibility: irreversible
        }
    }

    #[test]
    fn a_scrambled_key_order_expands_to_the_same_envelope() {
        assert_eq!(ScrambledOrder.name, "ScrambledOrder");
        assert!(ScrambledOrder.allows_write("Refund"));
        assert!(ScrambledOrder.allows_write("refund_notes"));
        assert!(ScrambledOrder.allows_unbounded_write("StaleDraft"));
        assert!(ScrambledOrder.allows_cross_tenant());
        assert!(ScrambledOrder.allows_outbound("https://api.stripe.com/v1/refunds/re_1"));
        assert!(ScrambledOrder.allows_outbound("alias:stripe"));
        assert!(ScrambledOrder.allows_webhook("refund.drafted"));
        assert!(ScrambledOrder.allows_job("NotifyFinance"));
        assert!(ScrambledOrder.allows_job("audit_export"));
        assert_eq!(ScrambledOrder.rate, Some("10/min"));
        assert_eq!(ScrambledOrder.spend, Some("500.00 USD"));
        assert_eq!(ScrambledOrder.reversibility, Reversibility::Compensable);
        assert!(ScrambledOrder.location.contains("agent_authority/mod.rs"));
    }

    #[test]
    fn a_grant_with_only_reversibility_denies_every_dimension() {
        assert_eq!(MinimalGrant.name, "MinimalGrant");
        assert!(!MinimalGrant.allows_write("Refund"));
        assert!(!MinimalGrant.allows_unbounded_write("Refund"));
        assert!(!MinimalGrant.allows_cross_tenant());
        assert!(!MinimalGrant.allows_outbound("https://api.stripe.com/"));
        assert!(!MinimalGrant.allows_webhook("refund.drafted"));
        assert!(!MinimalGrant.allows_job("NotifyFinance"));
        assert_eq!(MinimalGrant.rate, None);
        assert_eq!(MinimalGrant.spend, None);
        assert_eq!(MinimalGrant.reversibility, Reversibility::Reversible);
    }

    #[test]
    fn a_single_tenant_grant_does_not_treat_crossing_tenants_as_a_violation() {
        assert_eq!(SingleTenantSweeper.tenant_scope, TenantScope::None);
        assert!(SingleTenantSweeper.allows_cross_tenant());
        assert!(SingleTenantSweeper.writes.is_empty());
        assert_eq!(
            SingleTenantSweeper.reversibility,
            Reversibility::Irreversible
        );
    }

    #[test]
    fn every_declared_grant_reaches_the_manifest_through_inventory() {
        let names: Vec<&str> = inventory::iter::<GrantDescriptor>
            .into_iter()
            .map(|descriptor| descriptor.0.name)
            .collect();
        for expected in ["ScrambledOrder", "MinimalGrant", "SingleTenantSweeper"] {
            assert!(
                names.contains(&expected),
                "`authority_grant!` must register `{expected}`: {names:?}"
            );
        }
    }

    /// The checks the macro can only make once the whole declaration is in
    /// hand run at const-eval, so they are compile failures rather than test
    /// failures. `autumn/tests/compile-fail/agent_authority_*.rs` pins the
    /// messages; this test pins the predicates they rest on, which is the part
    /// that can drift silently.
    #[test]
    fn the_const_eval_gates_agree_with_the_grammar_they_advertise() {
        assert!(rate_is_wellformed("10/min"));
        assert!(!rate_is_wellformed("10/fortnight"));
        assert!(spend_is_wellformed("500.00 USD"));
        assert!(!spend_is_wellformed("500.00 usd"));
    }
}
