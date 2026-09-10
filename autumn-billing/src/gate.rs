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

use std::marker::PhantomData;
use std::sync::Arc;

use autumn_web::{AppState, AutumnError};
use axum_core_reexport::FromRequestParts;
use serde::{Deserialize, Serialize};

use crate::BillingService;
use crate::error::BillingError;
use crate::model::Subscription;
use crate::plan::{Plan, PlanId};

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
    /// # Errors
    ///
    /// Returns the store error.
    pub async fn current_subscription(
        &self,
        user_id: &str,
    ) -> Result<Option<SubscriptionView>, BillingError> {
        let _ = user_id;
        Err(BillingError::Unsupported("gate"))
    }

    /// `true` when `user_id` satisfies `rule`. Default deny.
    ///
    /// # Errors
    ///
    /// Returns the store error.
    pub async fn is_entitled(&self, user_id: &str, rule: &PlanRule) -> Result<bool, BillingError> {
        let _ = (user_id, rule);
        Ok(false)
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
        let _ = (user_id, rule);
        Err(BillingError::Forbidden("gate".to_owned()))
    }
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
        let _ = (parts, state);
        Err(AutumnError::forbidden_msg("gate"))
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
    let _ = (parts, state);
    Err(BillingError::Unauthenticated)
}
