//! Plans: the products a customer can subscribe to.
//!
//! Plans are configured in code or in `[[billing.plans]]`. There is no plans
//! table.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::ProviderId;
use crate::money::Money;

/// A plan identifier chosen by the application (for example `pro`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanId(String);

impl PlanId {
    /// Build a plan id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for PlanId {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

/// Billing interval of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BillingInterval {
    /// Every day.
    Day,
    /// Every week.
    Week,
    /// Every month.
    Month,
    /// Every year.
    Year,
}

/// A subscription plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Plan {
    /// Application id.
    pub id: PlanId,
    /// Display name.
    pub name: String,
    /// Provider price id (Stripe `price_...`).
    pub provider_price_id: ProviderId,
    /// Recurring price.
    pub price: Money,
    /// Billing interval.
    pub interval: BillingInterval,
    /// Entitlements granted by this plan (for example `export`).
    #[serde(default)]
    pub entitlements: BTreeSet<String>,
}

impl Plan {
    /// Build a plan with no entitlements.
    #[must_use]
    pub fn new(
        id: impl Into<PlanId>,
        name: impl Into<String>,
        provider_price_id: impl Into<ProviderId>,
        price: Money,
        interval: BillingInterval,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            provider_price_id: provider_price_id.into(),
            price,
            interval,
            entitlements: BTreeSet::new(),
        }
    }

    /// Add an entitlement.
    #[must_use]
    pub fn entitlement(mut self, entitlement: impl Into<String>) -> Self {
        self.entitlements.insert(entitlement.into());
        self
    }

    /// `true` when the plan grants `entitlement`.
    #[must_use]
    pub fn grants(&self, entitlement: &str) -> bool {
        self.entitlements.contains(entitlement)
    }
}

/// The set of plans an application sells.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanCatalog {
    by_id: BTreeMap<PlanId, Plan>,
    by_price: BTreeMap<ProviderId, PlanId>,
}

impl PlanCatalog {
    /// Build an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a plan. A later plan with the same id or price id replaces the earlier one.
    #[must_use]
    pub fn plan(mut self, plan: Plan) -> Self {
        self.insert(plan);
        self
    }

    /// Add a plan in place.
    pub fn insert(&mut self, plan: Plan) {
        self.by_price
            .insert(plan.provider_price_id.clone(), plan.id.clone());
        self.by_id.insert(plan.id.clone(), plan);
    }

    /// Look up a plan by id.
    #[must_use]
    pub fn get(&self, id: &PlanId) -> Option<&Plan> {
        self.by_id.get(id)
    }

    /// Look up a plan by provider price id.
    #[must_use]
    pub fn by_price_id(&self, price_id: &ProviderId) -> Option<&Plan> {
        self.by_price
            .get(price_id)
            .and_then(|id| self.by_id.get(id))
    }

    /// All plans, ordered by id.
    pub fn plans(&self) -> impl Iterator<Item = &Plan> {
        self.by_id.values()
    }

    /// `true` when the catalog has no plans.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}
