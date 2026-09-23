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

/// Separator between the tenant and the raw session user id in a
/// tenant-scoped billing identity ([`scope_identity_to_tenant`]).
///
/// A C0 control character rather than a printable one (`:`, `/`, …): a
/// tenant id (resolved by Autumn's own `[tenancy]` config — a header
/// allow-list, a subdomain map, a JWT claim) or an ordinary application user
/// id can spell any printable character, so only a byte neither can contain
/// makes `{tenant}{SEP}{user_id}` unambiguous to split back apart — the same
/// reasoning `idempotency.rs`'s length-prefixed key components exist for.
/// Kept as a plain separator rather than a hash so [`BillingHooks::recipient_for`](crate::hooks::BillingHooks::recipient_for)'s
/// default implementation can still recover the raw id (see there).
pub(crate) const TENANT_IDENTITY_SEPARATOR: char = '\u{1}';

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
/// receives — is now `{tenant}{TENANT_IDENTITY_SEPARATOR}{user_id}`, not the
/// bare session id. See that hook's doc for how to recover the raw id.
async fn user_id_in(session: &Session, state: &AppState) -> Result<String, BillingError> {
    let user_id = session
        .get(state.auth_session_key())
        .await
        .filter(|user_id| !user_id.is_empty())
        .ok_or(BillingError::Unauthenticated)?;
    Ok(scope_identity_to_tenant(user_id))
}

/// Recover the raw session user id from a billing identity
/// [`scope_identity_to_tenant`] may have tenant-scoped.
///
/// A no-op when tenancy is disabled, or for an identity written before this
/// existed (no separator present) — so it is safe to call unconditionally,
/// as [`BillingHooks::recipient_for`](crate::hooks::BillingHooks::recipient_for)'s
/// default implementation does. `TENANT_IDENTITY_SEPARATOR` itself stays
/// `pub(crate)`: this function, not the raw separator, is the stable surface
/// a custom `recipient_for` override recovers the bare id through.
#[must_use]
pub fn strip_tenant_scope(user_id: &str) -> &str {
    user_id
        .rsplit(TENANT_IDENTITY_SEPARATOR)
        .next()
        .unwrap_or(user_id)
}

/// Fold the request's ambient `CURRENT_TENANT` into a billing identity.
fn scope_identity_to_tenant(user_id: String) -> String {
    let tenant = autumn_web::tenancy::CURRENT_TENANT
        .try_with(Clone::clone)
        .ok()
        .flatten();
    match tenant {
        Some(tenant) => format!("{tenant}{TENANT_IDENTITY_SEPARATOR}{user_id}"),
        None => user_id,
    }
}
