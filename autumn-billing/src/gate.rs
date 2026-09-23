// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]
//! The plan gate: read local mirror state, default deny.
//!
//! A user is entitled when the mirror holds a subscription that is
//! `active` or `trialing` (`past_due` only with `allow_past_due`), whose
//! price maps to a catalog plan, and whose `current_period_end` plus the
//! grace period is not in the past. Without a known period end the grace
//! period counts from the last event applied. Every missing piece denies.

use std::marker::PhantomData;
use std::sync::Arc;

use autumn_web::session::Session;
use autumn_web::{AppState, AutumnError};
use axum_core_reexport::FromRequestParts;
use serde::{Deserialize, Serialize};

use crate::BillingService;
use crate::error::BillingError;
use crate::model::{Subscription, SubscriptionStatus};
use crate::plan::{Plan, PlanCatalog, PlanId};

mod axum_core_reexport {
    pub use autumn_web::reexports::axum::extract::FromRequestParts;
}

/// What a route requires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PlanRule {
    /// Any entitled subscription on any catalog plan.
    AnyActive,
    /// An entitled subscription on this plan.
    Plan(PlanId),
    /// An entitled subscription on a plan that grants this entitlement.
    Entitlement(String),
}

impl PlanRule {
    /// Require a plan.
    #[must_use]
    pub fn plan(id: impl Into<PlanId>) -> Self {
        Self::Plan(id.into())
    }

    /// Require an entitlement.
    #[must_use]
    pub fn entitlement(name: impl Into<String>) -> Self {
        Self::Entitlement(name.into())
    }

    /// `true` when `plan` satisfies the rule.
    fn accepts(&self, plan: &Plan) -> bool {
        match self {
            Self::AnyActive => true,
            Self::Plan(id) => &plan.id == id,
            Self::Entitlement(name) => plan.grants(name),
        }
    }

    /// Text for the `Forbidden` error.
    fn describe(&self) -> String {
        match self {
            Self::AnyActive => "an active subscription".to_owned(),
            Self::Plan(id) => format!("plan {id}"),
            Self::Entitlement(name) => format!("entitlement {name}"),
        }
    }
}

/// A compile-time plan requirement for [`Entitled`].
pub trait PlanRequirement: Send + Sync + 'static {
    /// The rule.
    fn rule() -> PlanRule;
}

/// A subscription joined with its plan, from the local mirror.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubscriptionView {
    /// The mirror row.
    pub subscription: Subscription,
    /// The catalog plan, when the price id is known.
    pub plan: Option<Plan>,
    /// `true` when the gate treats this subscription as entitled.
    pub entitled: bool,
}

impl SubscriptionView {
    /// Build a view. `entitled` is the caller's decision.
    #[must_use]
    pub const fn new(subscription: Subscription, plan: Option<Plan>, entitled: bool) -> Self {
        Self {
            subscription,
            plan,
            entitled,
        }
    }

    /// `true` when the view is entitled and its plan satisfies `rule`.
    fn satisfies(&self, rule: &PlanRule) -> bool {
        self.entitled && self.plan.as_ref().is_some_and(|plan| rule.accepts(plan))
    }
}

/// Service handle for handlers and policies.
#[derive(Clone)]
pub struct Billing {
    service: Arc<BillingService>,
    state: AppState,
}

impl std::fmt::Debug for Billing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Billing").finish_non_exhaustive()
    }
}

impl Billing {
    /// Build from state. `None` when the plugin did not start.
    #[must_use]
    pub fn from_state(state: &AppState) -> Option<Self> {
        BillingService::from_state(state).map(|service| Self {
            service,
            state: state.clone(),
        })
    }

    /// The service.
    #[must_use]
    pub fn service(&self) -> &BillingService {
        &self.service
    }

    /// The app state.
    #[must_use]
    pub const fn state(&self) -> &AppState {
        &self.state
    }

    /// The logged-in user id in `session`, read with the configured auth
    /// session key.
    ///
    /// Under Autumn's tenancy feature, this is the same opaque, tenant-scoped
    /// identity every billing store lookup is keyed on — not the bare
    /// session value — since two different tenants' sessions can otherwise
    /// stringify to the identical id (see `docs/guide/sharding.md`). Recover
    /// the raw id with [`crate::gate::strip_tenant_scope`] if a caller needs
    /// it. `CustomerRequest.user_id` (what a [`BillingProvider`](crate::provider::BillingProvider)
    /// receives) is unaffected: `customer_for` recovers the raw id before
    /// building that request, so a custom provider sees the same value as
    /// before tenancy folded anything in here.
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Unauthenticated`] when no user is logged in.
    pub async fn current_user(&self, session: &Session) -> Result<String, BillingError> {
        user_id_in(session, &self.state).await
    }

    /// The user's current subscription from the mirror. No provider call.
    ///
    /// Picks an entitled subscription first, then a live one, then the
    /// newest event within that group. An abandoned `incomplete` checkout
    /// never hides an active subscription.
    ///
    /// # Errors
    ///
    /// Returns the store error.
    pub async fn current_subscription(
        &self,
        user_id: &str,
    ) -> Result<Option<SubscriptionView>, BillingError> {
        let store = self.service.store();
        let Some(customer) = store.customer_by_user(user_id).await? else {
            return Ok(None);
        };
        let rows = store.subscriptions_for_customer(&customer.id).await?;
        Ok(rows
            .into_iter()
            .map(|row| self.view(row))
            .max_by_key(|view| {
                (
                    view.entitled,
                    view.subscription.status.is_live(),
                    view.subscription.last_event_at,
                )
            }))
    }

    /// `true` when `user_id` satisfies `rule`. Default deny.
    ///
    /// # Errors
    ///
    /// Returns the store error.
    pub async fn is_entitled(&self, user_id: &str, rule: &PlanRule) -> Result<bool, BillingError> {
        Ok(self
            .current_subscription(user_id)
            .await?
            .is_some_and(|view| view.satisfies(rule)))
    }

    /// The entitled subscription, or [`BillingError::Forbidden`].
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Forbidden`] when the rule is not satisfied.
    pub async fn require(
        &self,
        user_id: &str,
        rule: &PlanRule,
    ) -> Result<SubscriptionView, BillingError> {
        match self.current_subscription(user_id).await? {
            Some(view) if view.satisfies(rule) => Ok(view),
            _ => Err(BillingError::Forbidden(rule.describe())),
        }
    }

    /// Join the plan and evaluate entitlement.
    fn view(&self, subscription: Subscription) -> SubscriptionView {
        let plan = resolve_plan(self.service.catalog(), &subscription).cloned();
        let entitled =
            plan.is_some() && self.status_ok(subscription.status) && self.in_period(&subscription);
        SubscriptionView::new(subscription, plan, entitled)
    }

    fn status_ok(&self, status: SubscriptionStatus) -> bool {
        match status {
            SubscriptionStatus::Active | SubscriptionStatus::Trialing => true,
            SubscriptionStatus::PastDue => self.service.config().allow_past_due,
            _ => false,
        }
    }

    /// `true` when `current_period_end + grace` is not in the past. Without
    /// a period end the deadline is `last_event_at + grace`: a mirror the
    /// provider stopped feeding lapses either way.
    fn in_period(&self, subscription: &Subscription) -> bool {
        let end = subscription
            .current_period_end
            .unwrap_or(subscription.last_event_at);
        let grace = chrono::Duration::from_std(self.service.config().grace_period)
            .unwrap_or(chrono::Duration::MAX);
        let now = self.state.clock().now();
        end.checked_add_signed(grace)
            .is_none_or(|deadline| deadline >= now)
    }
}

/// The catalog plan for a mirror row: by price id first, then by stored plan id.
fn resolve_plan<'a>(catalog: &'a PlanCatalog, subscription: &Subscription) -> Option<&'a Plan> {
    subscription
        .provider_price_id
        .as_ref()
        .and_then(|price| catalog.by_price_id(price))
        .or_else(|| subscription.plan_id.as_ref().and_then(|id| catalog.get(id)))
}

impl FromRequestParts<AppState> for Billing {
    type Rejection = AutumnError;

    async fn from_request_parts(
        _parts: &mut autumn_web::reexports::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Self::from_state(state)
            .ok_or_else(|| AutumnError::service_unavailable_msg("billing plugin is not started"))
    }
}

/// Pre-body gate: 401 without a session user, 403 without an entitled
/// subscription that satisfies `R`.
pub struct Entitled<R: PlanRequirement> {
    /// The entitled subscription.
    pub view: SubscriptionView,
    _rule: PhantomData<R>,
}

impl<R: PlanRequirement> std::fmt::Debug for Entitled<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entitled")
            .field("view", &self.view)
            .finish()
    }
}

impl<R: PlanRequirement> FromRequestParts<AppState> for Entitled<R> {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut autumn_web::reexports::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user_id = session_user_id(parts, state)
            .await
            .map_err(BillingError::into_autumn)?;
        let billing = Billing::from_request_parts(parts, state).await?;
        let view = billing
            .require(&user_id, &R::rule())
            .await
            .map_err(BillingError::into_autumn)?;
        Ok(Self {
            view,
            _rule: PhantomData,
        })
    }
}

/// Resolve the session user id with the configured auth key.
///
/// # Errors
///
/// Returns [`BillingError::Unauthenticated`] when no user is logged in.
pub async fn session_user_id(
    parts: &mut autumn_web::reexports::http::request::Parts,
    state: &AppState,
) -> Result<String, BillingError> {
    let session = match Session::from_request_parts(parts, state).await {
        Ok(session) => session,
        Err(never) => match never {},
    };
    user_id_in(&session, state).await
}

/// Marker byte opening a tenant-scoped billing identity
/// (`scope_identity_to_tenant`).
///
/// A C0 control character rather than a printable one (`:`, `/`, …): an
/// ordinary application user id written before tenancy existed can spell any
/// printable character, so a byte it is vanishingly unlikely to already
/// start with is what lets [`strip_tenant_scope`] tell a scoped identity
/// apart from a pre-existing bare one. It is NOT what separates the tenant
/// from the user id within a scoped identity — `autumn_web::tenancy`'s
/// `session` and `jwt` sources hand back whatever string the app's own
/// session value or JWT claim contains, with no character-set restriction,
/// so a tenant string can itself contain this same byte. Splitting on it
/// (as an earlier version of this fix did) is then not injective: tenant `a`
/// with user id `b{MARKER}c` and tenant `a{MARKER}b` with user id `c` fold
/// to the identical string, reopening the exact cross-tenant collision this
/// scoping exists to close. `scope_identity_to_tenant` instead
/// length-prefixes the tenant — the same reasoning `idempotency.rs`'s
/// length-prefixed key components exist for, kept unhashed here so
/// [`BillingHooks::recipient_for`](crate::hooks::BillingHooks::recipient_for)'s
/// default implementation can still recover the raw id.
pub(crate) const TENANT_IDENTITY_MARKER: char = '\u{1}';

/// The non-empty user id stored under the configured auth session key,
/// scoped to the ambient tenant when one is in scope.
///
/// A session's stored identity (whatever the app's login handler put under
/// `auth.session_key`) is only guaranteed unique WITHIN its own tenant: a
/// `#[repository(tenant_scoped)]` model's row id is a per-tenant sequence,
/// and a sharded deployment's shard-local `BIGSERIAL` starts over on every
/// shard (`docs/guide/sharding.md`: the destination's PK sequence is never
/// copied between shards), so two different tenants' principals routinely
/// stringify to the identical `user_id`. Every billing store lookup is keyed
/// on this string alone (`customer_by_user`, `Entitled<R>`), and
/// `BillingPlugin` always resolves the app's one primary connection pool —
/// never a per-shard one — so without folding the tenant in here, two
/// unrelated principals that happen to share a `user_id` would be treated as
/// one and the same billing customer: reading, and through the hosted
/// portal potentially managing, each other's subscription.
///
/// `None` (tenancy disabled, or a `[tenancy] public_paths` route the
/// middleware exempts before it scopes anything) folds in nothing, so a
/// non-tenant app's stored identity — and therefore its `billing_customers`
/// rows — is byte-identical to what it was before this existed. Mirrors the
/// tenant-folding already applied to `#[cached]`'s cache key and the
/// idempotency/rate-limit storage keys.
///
/// **Compatibility:** under tenancy, this — and therefore `Customer.user_id`
/// and whatever [`BillingHooks::recipient_for`](crate::hooks::BillingHooks::recipient_for)
/// receives — is now an opaque, tenant-scoped identity, not the bare session
/// id. Recover the raw id with [`strip_tenant_scope`]; a pre-existing
/// `billing_customers` row keyed by the old bare id needs
/// [`BillingStore::relink_customer`](crate::store::BillingStore::relink_customer)
/// to move it onto the new one (see the migration guide).
async fn user_id_in(session: &Session, state: &AppState) -> Result<String, BillingError> {
    let user_id = session
        .get(state.auth_session_key())
        .await
        .filter(|user_id| !user_id.is_empty())
        .ok_or(BillingError::Unauthenticated)?;
    Ok(scope_identity_to_tenant(user_id))
}

/// Build the tenant-scoped billing identity for `tenant`/`user_id` directly,
/// without reading the ambient [`CURRENT_TENANT`](autumn_web::tenancy::CURRENT_TENANT).
///
/// For operator tooling only — an offline migration script has no request to
/// resolve a tenant from. Request-handling code goes through
/// [`session_user_id`], which resolves `tenant` from `CURRENT_TENANT` itself.
#[must_use]
pub fn scope_identity(tenant: &str, user_id: impl Into<String>) -> String {
    encode_tenant_scope(tenant, &user_id.into())
}

/// Length-prefix `tenant` so the split point stays unambiguous no matter what
/// bytes `tenant` or `user_id` themselves contain: `{MARKER}{tenant.len()}:{tenant}{user_id}`.
/// [`strip_tenant_scope`] reads the decimal length up to the first `:` after
/// the marker, then skips exactly that many bytes — never the marker or a
/// `:` occurring anywhere inside `tenant` or `user_id`, since nothing before
/// that first `:` can come from either of them.
fn encode_tenant_scope(tenant: &str, user_id: &str) -> String {
    format!("{TENANT_IDENTITY_MARKER}{}:{tenant}{user_id}", tenant.len())
}

/// Recover the raw session user id from a billing identity
/// `scope_identity_to_tenant` may have tenant-scoped.
///
/// A no-op when tenancy is disabled, or for an identity written before this
/// existed, or for anything else that does not parse as this crate's own
/// encoding (no marker, no `:`-terminated length, or too short) — so it is
/// safe to call unconditionally, as
/// [`BillingHooks::recipient_for`](crate::hooks::BillingHooks::recipient_for)'s
/// default implementation does. Neither `TENANT_IDENTITY_MARKER` nor the
/// wire format is public API: this function is the stable surface a custom
/// `recipient_for` override recovers the bare id through.
#[must_use]
pub fn strip_tenant_scope(user_id: &str) -> &str {
    let Some(rest) = user_id.strip_prefix(TENANT_IDENTITY_MARKER) else {
        return user_id;
    };
    let Some((len, rest)) = rest.split_once(':') else {
        return user_id;
    };
    let Ok(tenant_len) = len.parse::<usize>() else {
        return user_id;
    };
    rest.get(tenant_len..).unwrap_or(user_id)
}

/// Fold the request's ambient `CURRENT_TENANT` into a billing identity.
fn scope_identity_to_tenant(user_id: String) -> String {
    let tenant = autumn_web::tenancy::CURRENT_TENANT
        .try_with(Clone::clone)
        .ok()
        .flatten();
    match tenant {
        Some(tenant) => encode_tenant_scope(&tenant, &user_id),
        None => user_id,
    }
}

#[cfg(test)]
mod tenant_scope_tests {
    use super::{scope_identity, strip_tenant_scope};

    /// `strip_tenant_scope` recovers exactly what `scope_identity` encoded.
    #[test]
    fn round_trips() {
        let scoped = scope_identity("acme", "7");
        assert_eq!(strip_tenant_scope(&scoped), "7");
    }

    /// A separator-based encoding would fold tenant `"a"`/user `"b\u{1}c"`
    /// and tenant `"a\u{1}b"`/user `"c"` to the identical string — Autumn's
    /// `session`/`jwt` tenancy sources place no character-set restriction on
    /// the resolved tenant, so this is not a hypothetical input. The
    /// length-prefixed encoding must tell the two apart.
    #[test]
    fn injective_even_when_a_component_contains_the_marker() {
        let a = scope_identity("a", "b\u{1}c");
        let b = scope_identity("a\u{1}b", "c");
        assert_ne!(a, b, "distinct (tenant, user_id) pairs must not collide");
        assert_eq!(strip_tenant_scope(&a), "b\u{1}c");
        assert_eq!(strip_tenant_scope(&b), "c");
    }

    /// A colon right after the length digits — inside the tenant, not as the
    /// length/tenant delimiter — must not confuse the parse.
    #[test]
    fn tenant_containing_a_colon_still_round_trips() {
        let scoped = scope_identity("ten:ant", "user:42");
        assert_eq!(strip_tenant_scope(&scoped), "user:42");
    }

    /// A bare id written before tenancy existed (no marker) passes through
    /// unchanged rather than being misparsed.
    #[test]
    fn legacy_bare_id_is_unaffected() {
        assert_eq!(strip_tenant_scope("42"), "42");
    }

    /// Malformed input that merely starts with the marker (truncated,
    /// non-numeric length, or a length longer than what follows) degrades to
    /// returning the input unchanged rather than panicking on a bad slice
    /// index.
    #[test]
    fn malformed_scoped_looking_input_does_not_panic() {
        assert_eq!(strip_tenant_scope("\u{1}"), "\u{1}");
        assert_eq!(strip_tenant_scope("\u{1}abc:x"), "\u{1}abc:x");
        assert_eq!(strip_tenant_scope("\u{1}999:short"), "\u{1}999:short");
        assert_eq!(strip_tenant_scope("\u{1}3:ab"), "\u{1}3:ab");
    }
}
