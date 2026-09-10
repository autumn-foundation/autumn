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

    /// Add a plan in place. A later plan with the same id or price id
    /// replaces the earlier one; the replaced plan's price id stops resolving.
    pub fn insert(&mut self, plan: Plan) {
        if let Some(old) = self.by_id.get(&plan.id)
            && self.by_price.get(&old.provider_price_id) == Some(&old.id)
        {
            self.by_price.remove(&old.provider_price_id);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Currency;

    fn pro() -> Plan {
        Plan::new(
            "pro",
            "Pro",
            "price_pro",
            Money::from_minor(1999, Currency::USD),
            BillingInterval::Month,
        )
        .entitlement("export")
    }

    fn team() -> Plan {
        Plan::new(
            "team",
            "Team",
            "price_team",
            Money::from_minor(4999, Currency::USD),
            BillingInterval::Month,
        )
        .entitlement("export")
        .entitlement("sso")
    }

    #[test]
    fn catalog_lookup_by_id_and_price_id() {
        let catalog = PlanCatalog::new().plan(pro()).plan(team());
        assert!(!catalog.is_empty());
        assert_eq!(catalog.get(&PlanId::new("pro")), Some(&pro()));
        assert_eq!(catalog.get(&"team".into()), Some(&team()));
        assert_eq!(catalog.get(&PlanId::new("free")), None);
        assert_eq!(
            catalog.by_price_id(&ProviderId::new("price_team")),
            Some(&team())
        );
        assert_eq!(catalog.by_price_id(&ProviderId::new("price_nope")), None);
        let ids: Vec<&str> = catalog.plans().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["pro", "team"]);
    }

    #[test]
    fn empty_catalog() {
        let catalog = PlanCatalog::default();
        assert!(catalog.is_empty());
        assert_eq!(catalog.plans().count(), 0);
        assert_eq!(catalog.get(&PlanId::new("pro")), None);
    }

    #[test]
    fn later_plan_with_same_id_replaces_earlier() {
        let renamed = Plan::new(
            "pro",
            "Pro v2",
            "price_pro_v2",
            Money::from_minor(2999, Currency::USD),
            BillingInterval::Year,
        );
        let mut catalog = PlanCatalog::new().plan(pro());
        catalog.insert(renamed.clone());
        assert_eq!(catalog.plans().count(), 1);
        assert_eq!(catalog.get(&PlanId::new("pro")), Some(&renamed));
        assert_eq!(
            catalog.by_price_id(&ProviderId::new("price_pro_v2")),
            Some(&renamed)
        );
        // The old price id no longer resolves to a plan.
        assert_eq!(catalog.by_price_id(&ProviderId::new("price_pro")), None);
    }

    #[test]
    fn later_plan_with_same_price_id_replaces_earlier() {
        let clone = Plan::new(
            "pro_clone",
            "Pro clone",
            "price_pro",
            Money::from_minor(1999, Currency::USD),
            BillingInterval::Month,
        );
        let catalog = PlanCatalog::new().plan(pro()).plan(clone.clone());
        assert_eq!(
            catalog.by_price_id(&ProviderId::new("price_pro")),
            Some(&clone)
        );
        // Both ids stay addressable; the price maps to the latest plan.
        assert_eq!(catalog.get(&PlanId::new("pro")), Some(&pro()));
        assert_eq!(catalog.get(&PlanId::new("pro_clone")), Some(&clone));
    }

    #[test]
    fn grants_checks_entitlements() {
        let plan = team();
        assert!(plan.grants("export"));
        assert!(plan.grants("sso"));
        assert!(!plan.grants("Export"));
        assert!(!plan.grants("admin"));
        assert!(!pro().grants("sso"));
        let bare = Plan::new(
            "free",
            "Free",
            "price_free",
            Money::zero(Currency::USD),
            BillingInterval::Month,
        );
        assert!(bare.entitlements.is_empty());
        assert!(!bare.grants("export"));
    }

    #[test]
    fn plan_id_display_and_conversions() {
        let id = PlanId::from("pro");
        assert_eq!(id.as_str(), "pro");
        assert_eq!(id.to_string(), "pro");
        assert_eq!(id, PlanId::new(String::from("pro")));
        assert!(PlanId::new("a") < PlanId::new("b"));
    }

    #[test]
    fn plan_id_serde_is_transparent() {
        assert_eq!(
            serde_json::to_string(&PlanId::new("pro")).unwrap(),
            r#""pro""#
        );
        let id: PlanId = serde_json::from_str(r#""team""#).unwrap();
        assert_eq!(id, PlanId::new("team"));
    }

    #[test]
    fn billing_interval_serde_is_snake_case() {
        for (interval, text) in [
            (BillingInterval::Day, r#""day""#),
            (BillingInterval::Week, r#""week""#),
            (BillingInterval::Month, r#""month""#),
            (BillingInterval::Year, r#""year""#),
        ] {
            assert_eq!(serde_json::to_string(&interval).unwrap(), text);
            let back: BillingInterval = serde_json::from_str(text).unwrap();
            assert_eq!(back, interval);
        }
        assert!(serde_json::from_str::<BillingInterval>(r#""Month""#).is_err());
        assert!(serde_json::from_str::<BillingInterval>(r#""quarter""#).is_err());
    }

    #[test]
    fn plan_serde_round_trip() {
        let plan = team();
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id": "team",
                "name": "Team",
                "provider_price_id": "price_team",
                "price": { "minor": 4999, "currency": "USD" },
                "interval": "month",
                "entitlements": ["export", "sso"],
            })
        );
        let back: Plan = serde_json::from_value(json).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn plan_serde_entitlements_default_to_empty() {
        let plan: Plan = serde_json::from_str(
            r#"{
                "id": "pro",
                "name": "Pro",
                "provider_price_id": "price_pro",
                "price": { "minor": 1999, "currency": "usd" },
                "interval": "year"
            }"#,
        )
        .unwrap();
        assert_eq!(plan.id, PlanId::new("pro"));
        assert_eq!(plan.price, Money::from_minor(1999, Currency::USD));
        assert_eq!(plan.interval, BillingInterval::Year);
        assert!(plan.entitlements.is_empty());
    }
}
