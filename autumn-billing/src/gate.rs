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
//! grace period is not in the past. Every missing piece denies.

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

    /// The user's current subscription from the mirror. No provider call.
    ///
    /// Picks the live subscription with the newest event, else the newest
    /// row of any status.
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
        let live = rows
            .iter()
            .filter(|row| row.status.is_live())
            .max_by_key(|row| row.last_event_at);
        let best = live.or_else(|| rows.iter().max_by_key(|row| row.last_event_at));
        Ok(best.map(|row| self.view(row.clone())))
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
        SubscriptionView {
            subscription,
            plan,
            entitled,
        }
    }

    fn status_ok(&self, status: SubscriptionStatus) -> bool {
        match status {
            SubscriptionStatus::Active | SubscriptionStatus::Trialing => true,
            SubscriptionStatus::PastDue => self.service.config().allow_past_due,
            _ => false,
        }
    }

    /// `true` when `current_period_end + grace` is not in the past, or no
    /// period end is known.
    fn in_period(&self, subscription: &Subscription) -> bool {
        let Some(end) = subscription.current_period_end else {
            return true;
        };
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
    session
        .get(state.auth_session_key())
        .await
        .filter(|user_id| !user_id.is_empty())
        .ok_or(BillingError::Unauthenticated)
}
