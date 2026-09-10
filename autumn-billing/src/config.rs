//! `[billing]` configuration.
//!
//! Config is resolved after `Plugin::build`, so the application loads a
//! [`BillingConfig`] up front (`from_autumn_toml` or `from_env`) and passes it
//! to `BillingPlugin::config`. Secrets come from the environment; env values
//! win over TOML values.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::Path;
use std::time::Duration;

use autumn_web::webhook::WebhookEndpointConfig;
use serde::{Deserialize, Serialize};

use crate::error::BillingError;
use crate::model::ProviderId;
use crate::money::{Currency, Money};
use crate::plan::{BillingInterval, Plan, PlanId};

/// Env var that holds the Stripe API secret key.
pub const STRIPE_SECRET_KEY_ENV: &str = "STRIPE_SECRET_KEY";
/// Env var that holds the Stripe webhook signing secret.
pub const STRIPE_WEBHOOK_SECRET_ENV: &str = "STRIPE_WEBHOOK_SECRET";
/// Default route prefix.
pub const DEFAULT_ROUTE_PREFIX: &str = "/billing";
/// Default name of the `security.webhooks.endpoints` entry.
pub const DEFAULT_ENDPOINT_NAME: &str = "billing";
/// Default Stripe API base URL.
pub const DEFAULT_STRIPE_API_BASE: &str = "https://api.stripe.com";
/// Env var that overrides `billing.stripe.api_base`.
pub const STRIPE_API_BASE_ENV: &str = "AUTUMN_BILLING__STRIPE__API_BASE";
/// Env var that overrides `billing.route_prefix`.
pub const ROUTE_PREFIX_ENV: &str = "AUTUMN_BILLING__ROUTE_PREFIX";

/// A secret that never prints.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a secret.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read the secret. Use only at the call that needs it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `true` when the secret is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// What to do when every retry failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExhaustionAction {
    /// Mark the mirror `unpaid` and cancel the subscription at the provider.
    CancelSubscription,
    /// Mark the mirror `unpaid` only. The provider keeps the subscription.
    MarkUnpaid,
}

/// Retry schedule for failed payments.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DunningPolicy {
    /// `false` mirrors provider events and notifies, but never retries.
    pub enabled: bool,
    /// Delay before retry `n`. Retry 1 counts from the failure. Later
    /// retries count from the previous retry. Length = number of retries.
    pub retry_delays: Vec<Duration>,
    /// Action after the last retry fails.
    pub on_exhausted: ExhaustionAction,
}

impl DunningPolicy {
    /// Three retries after 1, 3 and 5 days; cancel on exhaustion.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            enabled: true,
            retry_delays: vec![
                Duration::from_secs(86_400),
                Duration::from_secs(3 * 86_400),
                Duration::from_secs(5 * 86_400),
            ],
            on_exhausted: ExhaustionAction::CancelSubscription,
        }
    }

    /// Mirror only: notify on failure, never retry.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            retry_delays: Vec::new(),
            on_exhausted: ExhaustionAction::MarkUnpaid,
        }
    }

    /// Replace the retry delays.
    #[must_use]
    pub fn with_retry_delays(mut self, delays: Vec<Duration>) -> Self {
        self.retry_delays = delays;
        self
    }

    /// Replace the exhaustion action.
    #[must_use]
    pub const fn with_on_exhausted(mut self, action: ExhaustionAction) -> Self {
        self.on_exhausted = action;
        self
    }

    /// Delay before retry number `attempt` (1-based). `None` when exhausted.
    #[must_use]
    pub fn delay_for(&self, attempt: i64) -> Option<Duration> {
        let index = usize::try_from(attempt.checked_sub(1)?).ok()?;
        self.retry_delays.get(index).copied()
    }

    /// Number of retries.
    #[must_use]
    pub fn max_attempts(&self) -> i64 {
        i64::try_from(self.retry_delays.len()).unwrap_or(i64::MAX)
    }
}

impl Default for DunningPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

/// Stripe credentials and endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StripeConfig {
    /// API secret key (`sk_live_…` / `sk_test_…`).
    pub secret_key: Option<SecretString>,
    /// Webhook signing secret (`whsec_…`).
    pub webhook_secret: Option<SecretString>,
    /// API base URL.
    pub api_base: String,
}

impl Default for StripeConfig {
    fn default() -> Self {
        Self {
            secret_key: None,
            webhook_secret: None,
            api_base: DEFAULT_STRIPE_API_BASE.to_owned(),
        }
    }
}

/// The `[billing]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BillingConfig {
    /// URL prefix of the plugin routes.
    pub route_prefix: String,
    /// Name of the `security.webhooks.endpoints` entry.
    pub endpoint_name: String,
    /// Browser redirect after a successful checkout.
    pub success_url: String,
    /// Browser redirect after a canceled checkout.
    pub cancel_url: String,
    /// Browser redirect when the customer leaves the portal.
    pub portal_return_url: String,
    /// Treat `past_due` as entitled.
    pub allow_past_due: bool,
    /// Entitlement survives this long past `current_period_end` without a
    /// renewal event. Guards a dead webhook.
    pub grace_period: Duration,
    /// Retry schedule.
    pub dunning: DunningPolicy,
    /// Stripe settings.
    pub stripe: StripeConfig,
    /// Plans declared in `[[billing.plans]]`.
    pub plans: Vec<Plan>,
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            route_prefix: DEFAULT_ROUTE_PREFIX.to_owned(),
            endpoint_name: DEFAULT_ENDPOINT_NAME.to_owned(),
            success_url: "/billing/success".to_owned(),
            cancel_url: "/billing/cancel".to_owned(),
            portal_return_url: "/".to_owned(),
            allow_past_due: false,
            grace_period: Duration::from_secs(72 * 3600),
            dunning: DunningPolicy::standard(),
            stripe: StripeConfig::default(),
            plans: Vec::new(),
        }
    }
}

impl BillingConfig {
    /// Defaults plus secrets from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        let env: HashMap<String, String> = std::env::vars().collect();
        Self::default().with_env_pairs(&env)
    }

    /// Parse `[billing]` from an `autumn.toml` file, then apply the process
    /// environment.
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Config`] when the file cannot be read or parsed.
    pub fn from_autumn_toml(path: impl AsRef<Path>) -> Result<Self, BillingError> {
        let text = std::fs::read_to_string(path.as_ref())
            .map_err(|e| BillingError::Config(format!("read autumn.toml: {e}")))?;
        let env: HashMap<String, String> = std::env::vars().collect();
        Self::from_toml_str(&text).map(|cfg| cfg.with_env_pairs(&env))
    }

    /// Parse `[billing]` from TOML text. Missing section = defaults.
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Config`] when the TOML is not valid.
    pub fn from_toml_str(text: &str) -> Result<Self, BillingError> {
        let root: RootToml = toml::from_str(text)
            .map_err(|e| BillingError::Config(format!("parse [billing]: {e}")))?;
        Ok(root
            .billing
            .map_or_else(Self::default, BillingToml::into_config))
    }

    /// Apply env overrides. `STRIPE_SECRET_KEY` and `STRIPE_WEBHOOK_SECRET`
    /// win over TOML values, as do `AUTUMN_BILLING__STRIPE__API_BASE` and
    /// `AUTUMN_BILLING__ROUTE_PREFIX`. Blank values are ignored.
    #[must_use]
    pub fn with_env_pairs(mut self, env: &HashMap<String, String>) -> Self {
        if let Some(key) = non_blank(env, STRIPE_SECRET_KEY_ENV) {
            self.stripe.secret_key = Some(SecretString::new(key));
        }
        if let Some(secret) = non_blank(env, STRIPE_WEBHOOK_SECRET_ENV) {
            self.stripe.webhook_secret = Some(SecretString::new(secret));
        }
        if let Some(base) = non_blank(env, STRIPE_API_BASE_ENV) {
            base.clone_into(&mut self.stripe.api_base);
        }
        if let Some(prefix) = non_blank(env, ROUTE_PREFIX_ENV) {
            prefix.clone_into(&mut self.route_prefix);
        }
        self
    }

    /// Set the Stripe secret key.
    #[must_use]
    pub fn stripe_secret_key(mut self, key: impl Into<String>) -> Self {
        self.stripe.secret_key = Some(SecretString::new(key));
        self
    }

    /// Set the Stripe webhook secret.
    #[must_use]
    pub fn stripe_webhook_secret(mut self, secret: impl Into<String>) -> Self {
        self.stripe.webhook_secret = Some(SecretString::new(secret));
        self
    }

    /// Set the route prefix.
    #[must_use]
    pub fn route_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.route_prefix = prefix.into();
        self
    }

    /// Set the redirect URLs.
    #[must_use]
    pub fn urls(
        mut self,
        success: impl Into<String>,
        cancel: impl Into<String>,
        portal_return: impl Into<String>,
    ) -> Self {
        self.success_url = success.into();
        self.cancel_url = cancel.into();
        self.portal_return_url = portal_return.into();
        self
    }

    /// Set the dunning policy.
    #[must_use]
    pub fn dunning(mut self, policy: DunningPolicy) -> Self {
        self.dunning = policy;
        self
    }

    /// Treat `past_due` as entitled.
    #[must_use]
    pub const fn allow_past_due(mut self, allow: bool) -> Self {
        self.allow_past_due = allow;
        self
    }

    /// Set the grace period.
    #[must_use]
    pub const fn grace_period(mut self, grace: Duration) -> Self {
        self.grace_period = grace;
        self
    }

    /// The webhook route path (`{prefix}/webhook`).
    #[must_use]
    pub fn webhook_path(&self) -> String {
        format!("{}/webhook", self.route_prefix.trim_end_matches('/'))
    }

    /// The `security.webhooks.endpoints` entry for the Stripe receiver.
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Config`] when the webhook secret is missing.
    pub fn webhook_endpoint(&self) -> Result<WebhookEndpointConfig, BillingError> {
        let secret = self
            .stripe
            .webhook_secret
            .as_ref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                BillingError::Config(format!("{STRIPE_WEBHOOK_SECRET_ENV} is not set"))
            })?;
        let mut endpoint = WebhookEndpointConfig::stripe(
            self.endpoint_name.clone(),
            self.webhook_path(),
            secret.expose(),
        );
        // Invoices with many lines exceed the 1 MiB default.
        endpoint.max_body_bytes = 4 * 1024 * 1024;
        Ok(endpoint)
    }

    /// Check the config for `profile`. Production requires live keys.
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Config`] on the first problem found.
    pub fn validate(&self, is_production: bool) -> Result<(), BillingError> {
        if !self.route_prefix.starts_with('/') || self.route_prefix == "/" {
            return Err(BillingError::Config(format!(
                "route_prefix must start with '/' and name a sub-path, got {:?}",
                self.route_prefix
            )));
        }
        for (name, url) in [
            ("success_url", &self.success_url),
            ("cancel_url", &self.cancel_url),
            ("portal_return_url", &self.portal_return_url),
        ] {
            if url.trim().is_empty() {
                return Err(BillingError::Config(format!("{name} must not be empty")));
            }
        }
        if self.dunning.enabled && self.dunning.retry_delays.is_empty() {
            return Err(BillingError::Config(
                "dunning.retry_delays must not be empty when dunning is enabled".to_owned(),
            ));
        }
        if !is_production {
            return Ok(());
        }
        let secret_key = present(self.stripe.secret_key.as_ref()).ok_or_else(|| {
            BillingError::Config(format!(
                "{STRIPE_SECRET_KEY_ENV} is not set; production requires a live Stripe key"
            ))
        })?;
        if secret_key.expose().starts_with("sk_test_") {
            return Err(BillingError::Config(format!(
                "{STRIPE_SECRET_KEY_ENV} is a sk_test_ key; production requires a live Stripe key"
            )));
        }
        present(self.stripe.webhook_secret.as_ref()).ok_or_else(|| {
            BillingError::Config(format!(
                "{STRIPE_WEBHOOK_SECRET_ENV} is not set; production requires a webhook secret"
            ))
        })?;
        Ok(())
    }
}

/// The trimmed env value, or `None` when unset or blank.
fn non_blank<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// The secret, or `None` when unset or empty.
fn present(secret: Option<&SecretString>) -> Option<&SecretString> {
    secret.filter(|s| !s.is_empty())
}

/// Seconds in one hour.
const SECS_PER_HOUR: u64 = 3600;

const fn hours(value: u64) -> Duration {
    Duration::from_secs(value.saturating_mul(SECS_PER_HOUR))
}

/// The whole `autumn.toml`. Other sections are ignored.
#[derive(Deserialize)]
struct RootToml {
    #[serde(default)]
    billing: Option<BillingToml>,
}

/// The `[billing]` table as written. Every key is optional.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BillingToml {
    route_prefix: Option<String>,
    endpoint_name: Option<String>,
    success_url: Option<String>,
    cancel_url: Option<String>,
    portal_return_url: Option<String>,
    allow_past_due: Option<bool>,
    grace_period_hours: Option<u64>,
    dunning: Option<DunningToml>,
    stripe: Option<StripeToml>,
    #[serde(default)]
    plans: Vec<PlanToml>,
}

impl BillingToml {
    fn into_config(self) -> BillingConfig {
        let defaults = BillingConfig::default();
        BillingConfig {
            route_prefix: self.route_prefix.unwrap_or(defaults.route_prefix),
            endpoint_name: self.endpoint_name.unwrap_or(defaults.endpoint_name),
            success_url: self.success_url.unwrap_or(defaults.success_url),
            cancel_url: self.cancel_url.unwrap_or(defaults.cancel_url),
            portal_return_url: self.portal_return_url.unwrap_or(defaults.portal_return_url),
            allow_past_due: self.allow_past_due.unwrap_or(defaults.allow_past_due),
            grace_period: self.grace_period_hours.map_or(defaults.grace_period, hours),
            dunning: self
                .dunning
                .map_or(defaults.dunning, DunningToml::into_policy),
            stripe: self.stripe.map_or(defaults.stripe, StripeToml::into_config),
            plans: self.plans.into_iter().map(PlanToml::into_plan).collect(),
        }
    }
}

/// `[billing.dunning]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DunningToml {
    enabled: Option<bool>,
    retry_delays_hours: Option<Vec<u64>>,
    on_exhausted: Option<ExhaustionAction>,
}

impl DunningToml {
    fn into_policy(self) -> DunningPolicy {
        let standard = DunningPolicy::standard();
        DunningPolicy {
            enabled: self.enabled.unwrap_or(standard.enabled),
            retry_delays: self.retry_delays_hours.map_or(standard.retry_delays, |h| {
                h.into_iter().map(hours).collect()
            }),
            on_exhausted: self.on_exhausted.unwrap_or(standard.on_exhausted),
        }
    }
}

/// `[billing.stripe]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StripeToml {
    secret_key: Option<SecretString>,
    webhook_secret: Option<SecretString>,
    api_base: Option<String>,
}

impl StripeToml {
    fn into_config(self) -> StripeConfig {
        StripeConfig {
            secret_key: self.secret_key,
            webhook_secret: self.webhook_secret,
            api_base: self
                .api_base
                .unwrap_or_else(|| DEFAULT_STRIPE_API_BASE.to_owned()),
        }
    }
}

/// One `[[billing.plans]]` entry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanToml {
    id: PlanId,
    name: String,
    price_id: String,
    amount_minor: i64,
    currency: Currency,
    interval: BillingInterval,
    #[serde(default)]
    entitlements: BTreeSet<String>,
}

impl PlanToml {
    fn into_plan(self) -> Plan {
        let mut plan = Plan::new(
            self.id,
            self.name,
            ProviderId::new(self.price_id),
            Money::from_minor(self.amount_minor, self.currency),
            self.interval,
        );
        plan.entitlements = self.entitlements;
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::{Currency, Money};
    use crate::plan::{BillingInterval, PlanId};
    use autumn_web::webhook::WebhookProvider;

    const FULL: &str = r#"
[server]
port = 3000

[billing]
route_prefix = "/pay"
endpoint_name = "stripe-billing"
success_url = "https://app.test/ok"
cancel_url = "https://app.test/cancel"
portal_return_url = "https://app.test/account"
allow_past_due = true
grace_period_hours = 24

[billing.dunning]
enabled = true
retry_delays_hours = [24, 72, 120]
on_exhausted = "mark_unpaid"

[billing.stripe]
secret_key = "sk_test_from_toml"
webhook_secret = "whsec_from_toml"
api_base = "http://localhost:12111"

[[billing.plans]]
id = "pro"
name = "Pro"
price_id = "price_pro"
amount_minor = 1999
currency = "usd"
interval = "month"
entitlements = ["export"]

[[billing.plans]]
id = "team"
name = "Team"
price_id = "price_team"
amount_minor = 49990
currency = "JPY"
interval = "year"
"#;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn err_message(result: Result<impl fmt::Debug, BillingError>) -> String {
        match result {
            Ok(value) => panic!("expected an error, got {value:?}"),
            Err(BillingError::Config(message)) => message,
            Err(other) => panic!("expected BillingError::Config, got {other:?}"),
        }
    }

    #[test]
    fn toml_full_section_round_trips() {
        let cfg = BillingConfig::from_toml_str(FULL).unwrap();
        assert_eq!(cfg.route_prefix, "/pay");
        assert_eq!(cfg.endpoint_name, "stripe-billing");
        assert_eq!(cfg.success_url, "https://app.test/ok");
        assert_eq!(cfg.cancel_url, "https://app.test/cancel");
        assert_eq!(cfg.portal_return_url, "https://app.test/account");
        assert!(cfg.allow_past_due);
        assert_eq!(cfg.grace_period, Duration::from_secs(24 * 3600));

        assert!(cfg.dunning.enabled);
        assert_eq!(
            cfg.dunning.retry_delays,
            vec![
                Duration::from_secs(24 * 3600),
                Duration::from_secs(72 * 3600),
                Duration::from_secs(120 * 3600),
            ]
        );
        assert_eq!(cfg.dunning.on_exhausted, ExhaustionAction::MarkUnpaid);

        assert_eq!(
            cfg.stripe.secret_key.as_ref().map(SecretString::expose),
            Some("sk_test_from_toml")
        );
        assert_eq!(
            cfg.stripe.webhook_secret.as_ref().map(SecretString::expose),
            Some("whsec_from_toml")
        );
        assert_eq!(cfg.stripe.api_base, "http://localhost:12111");

        assert_eq!(cfg.plans.len(), 2);
        let pro = &cfg.plans[0];
        assert_eq!(pro.id, PlanId::new("pro"));
        assert_eq!(pro.name, "Pro");
        assert_eq!(pro.provider_price_id.as_str(), "price_pro");
        assert_eq!(pro.price, Money::from_minor(1999, Currency::USD));
        assert_eq!(pro.interval, BillingInterval::Month);
        assert!(pro.grants("export"));
        let team = &cfg.plans[1];
        assert_eq!(team.id, PlanId::new("team"));
        assert_eq!(team.price, Money::from_minor(49_990, Currency::JPY));
        assert_eq!(team.interval, BillingInterval::Year);
        assert!(team.entitlements.is_empty());
    }

    #[test]
    fn toml_missing_section_is_default() {
        let cfg = BillingConfig::from_toml_str("[server]\nport = 3000\n").unwrap();
        assert_eq!(cfg, BillingConfig::default());
        let empty = BillingConfig::from_toml_str("").unwrap();
        assert_eq!(empty, BillingConfig::default());
    }

    #[test]
    fn toml_empty_section_is_default() {
        let cfg = BillingConfig::from_toml_str("[billing]\n").unwrap();
        assert_eq!(cfg, BillingConfig::default());
    }

    #[test]
    fn toml_partial_section_keeps_other_defaults() {
        let cfg = BillingConfig::from_toml_str(
            "[billing]\nallow_past_due = true\n[billing.dunning]\nenabled = false\n",
        )
        .unwrap();
        assert!(cfg.allow_past_due);
        assert_eq!(cfg.route_prefix, DEFAULT_ROUTE_PREFIX);
        assert_eq!(cfg.stripe, StripeConfig::default());
        assert!(!cfg.dunning.enabled);
        // Unset dunning fields keep the standard values.
        assert_eq!(
            cfg.dunning.retry_delays,
            DunningPolicy::standard().retry_delays
        );
        assert_eq!(
            cfg.dunning.on_exhausted,
            ExhaustionAction::CancelSubscription
        );
    }

    #[test]
    fn toml_unknown_key_is_rejected() {
        let message = err_message(BillingConfig::from_toml_str(
            "[billing]\nroute_prefx = \"/pay\"\n",
        ));
        assert!(message.contains("route_prefx"), "{message}");

        let nested = err_message(BillingConfig::from_toml_str(
            "[billing.stripe]\nsecret = \"sk_test_x\"\n",
        ));
        assert!(nested.contains("secret"), "{nested}");
    }

    #[test]
    fn toml_wrong_type_is_rejected() {
        let message = err_message(BillingConfig::from_toml_str(
            "[billing]\ngrace_period_hours = \"soon\"\n",
        ));
        assert!(message.contains("grace_period_hours"), "{message}");
    }

    #[test]
    fn toml_invalid_document_is_rejected() {
        err_message(BillingConfig::from_toml_str("[billing\n"));
    }

    #[test]
    fn toml_plan_with_bad_currency_is_rejected() {
        let text = "[[billing.plans]]\nid = \"pro\"\nname = \"Pro\"\nprice_id = \"p\"\n\
                    amount_minor = 1\ncurrency = \"dollars\"\ninterval = \"month\"\n";
        let message = err_message(BillingConfig::from_toml_str(text));
        assert!(message.contains("dollars"), "{message}");
    }

    #[test]
    fn env_wins_over_toml() {
        let cfg = BillingConfig::from_toml_str(FULL)
            .unwrap()
            .with_env_pairs(&env(&[
                (STRIPE_SECRET_KEY_ENV, "sk_live_from_env"),
                (STRIPE_WEBHOOK_SECRET_ENV, "whsec_from_env"),
                (
                    "AUTUMN_BILLING__STRIPE__API_BASE",
                    "http://stripe-mock:12111",
                ),
                ("AUTUMN_BILLING__ROUTE_PREFIX", "/subscriptions"),
                ("UNRELATED", "value"),
            ]));
        assert_eq!(
            cfg.stripe.secret_key.as_ref().map(SecretString::expose),
            Some("sk_live_from_env")
        );
        assert_eq!(
            cfg.stripe.webhook_secret.as_ref().map(SecretString::expose),
            Some("whsec_from_env")
        );
        assert_eq!(cfg.stripe.api_base, "http://stripe-mock:12111");
        assert_eq!(cfg.route_prefix, "/subscriptions");
        // Other TOML values are untouched.
        assert_eq!(cfg.endpoint_name, "stripe-billing");
        assert_eq!(cfg.plans.len(), 2);
    }

    #[test]
    fn env_fills_missing_secrets() {
        let cfg = BillingConfig::default().with_env_pairs(&env(&[
            (STRIPE_SECRET_KEY_ENV, "sk_test_abc"),
            (STRIPE_WEBHOOK_SECRET_ENV, "whsec_abc"),
        ]));
        assert_eq!(
            cfg.stripe.secret_key.as_ref().map(SecretString::expose),
            Some("sk_test_abc")
        );
        assert_eq!(
            cfg.stripe.webhook_secret.as_ref().map(SecretString::expose),
            Some("whsec_abc")
        );
    }

    #[test]
    fn blank_env_is_ignored() {
        let toml = BillingConfig::from_toml_str(FULL).unwrap();
        let cfg = toml.clone().with_env_pairs(&env(&[
            (STRIPE_SECRET_KEY_ENV, ""),
            (STRIPE_WEBHOOK_SECRET_ENV, "   "),
            ("AUTUMN_BILLING__STRIPE__API_BASE", ""),
            ("AUTUMN_BILLING__ROUTE_PREFIX", "\t"),
        ]));
        assert_eq!(cfg, toml);
        let untouched = toml.clone().with_env_pairs(&HashMap::new());
        assert_eq!(untouched, toml);
    }

    #[test]
    fn env_values_are_trimmed() {
        let cfg = BillingConfig::default()
            .with_env_pairs(&env(&[(STRIPE_SECRET_KEY_ENV, " sk_test_abc\n")]));
        assert_eq!(
            cfg.stripe.secret_key.as_ref().map(SecretString::expose),
            Some("sk_test_abc")
        );
    }

    fn live() -> BillingConfig {
        BillingConfig::default()
            .stripe_secret_key("sk_live_abc")
            .stripe_webhook_secret("whsec_abc")
    }

    #[test]
    fn validate_dev_accepts_test_key_and_missing_secrets() {
        BillingConfig::default().validate(false).unwrap();
        BillingConfig::default()
            .stripe_secret_key("sk_test_abc")
            .validate(false)
            .unwrap();
        live().validate(false).unwrap();
    }

    #[test]
    fn validate_production_accepts_live_keys() {
        live().validate(true).unwrap();
    }

    #[test]
    fn validate_production_rejects_test_key() {
        let cfg = live().stripe_secret_key("sk_test_abc");
        let message = err_message(cfg.validate(true));
        assert!(message.contains("sk_test_"), "{message}");
        assert!(!message.contains("sk_test_abc"), "leaks the key: {message}");
    }

    #[test]
    fn validate_production_rejects_missing_secret_key() {
        let cfg = BillingConfig::default().stripe_webhook_secret("whsec_abc");
        let message = err_message(cfg.validate(true));
        assert!(message.contains(STRIPE_SECRET_KEY_ENV), "{message}");
        let blank = live().stripe_secret_key("");
        let message = err_message(blank.validate(true));
        assert!(message.contains(STRIPE_SECRET_KEY_ENV), "{message}");
    }

    #[test]
    fn validate_production_rejects_missing_webhook_secret() {
        let cfg = BillingConfig::default().stripe_secret_key("sk_live_abc");
        let message = err_message(cfg.validate(true));
        assert!(message.contains(STRIPE_WEBHOOK_SECRET_ENV), "{message}");
        let blank = live().stripe_webhook_secret("");
        let message = err_message(blank.validate(true));
        assert!(message.contains(STRIPE_WEBHOOK_SECRET_ENV), "{message}");
    }

    #[test]
    fn validate_rejects_bad_route_prefix() {
        for bad in ["", "billing", "/"] {
            let message = err_message(live().route_prefix(bad).validate(false));
            assert!(message.contains("route_prefix"), "{bad:?}: {message}");
        }
        live().route_prefix("/pay").validate(false).unwrap();
    }

    #[test]
    fn validate_rejects_empty_urls() {
        let message = err_message(live().urls("", "/c", "/p").validate(false));
        assert!(message.contains("success_url"), "{message}");
        let message = err_message(live().urls("/s", "", "/p").validate(false));
        assert!(message.contains("cancel_url"), "{message}");
        let message = err_message(live().urls("/s", "/c", "").validate(false));
        assert!(message.contains("portal_return_url"), "{message}");
    }

    #[test]
    fn validate_rejects_enabled_dunning_without_delays() {
        let cfg = live().dunning(DunningPolicy::standard().with_retry_delays(Vec::new()));
        let message = err_message(cfg.validate(false));
        assert!(message.contains("retry_delays"), "{message}");
        live()
            .dunning(DunningPolicy::disabled())
            .validate(false)
            .unwrap();
    }

    #[test]
    fn webhook_path_shape() {
        assert_eq!(BillingConfig::default().webhook_path(), "/billing/webhook");
        assert_eq!(
            BillingConfig::default()
                .route_prefix("/pay/")
                .webhook_path(),
            "/pay/webhook"
        );
    }

    #[test]
    fn webhook_endpoint_shape() {
        let endpoint = live().webhook_endpoint().unwrap();
        assert_eq!(endpoint.name, DEFAULT_ENDPOINT_NAME);
        assert_eq!(endpoint.path, "/billing/webhook");
        assert_eq!(endpoint.provider, WebhookProvider::Stripe);
        assert_eq!(endpoint.secret.as_deref(), Some("whsec_abc"));
        assert_eq!(endpoint.max_body_bytes, 4 * 1024 * 1024);
    }

    #[test]
    fn webhook_endpoint_requires_secret() {
        let message = err_message(BillingConfig::default().webhook_endpoint());
        assert!(message.contains(STRIPE_WEBHOOK_SECRET_ENV), "{message}");
        let blank = BillingConfig::default().stripe_webhook_secret("");
        err_message(blank.webhook_endpoint());
    }

    #[test]
    fn secret_string_never_prints() {
        let secret = SecretString::new("sk_live_hunter2");
        assert!(!format!("{secret:?}").contains("hunter2"));
        assert!(!format!("{secret}").contains("hunter2"));
        assert_eq!(format!("{secret}"), "<redacted>");
        assert_eq!(secret.expose(), "sk_live_hunter2");
        assert!(!secret.is_empty());
        assert!(SecretString::new("").is_empty());

        let debug = format!("{:?}", live());
        assert!(!debug.contains("sk_live_abc"), "{debug}");
        assert!(!debug.contains("whsec_abc"), "{debug}");
    }

    #[test]
    fn dunning_delay_for_boundaries() {
        let policy = DunningPolicy::standard();
        assert_eq!(policy.max_attempts(), 3);
        assert_eq!(policy.delay_for(0), None);
        assert_eq!(policy.delay_for(-1), None);
        assert_eq!(policy.delay_for(i64::MIN), None);
        assert_eq!(policy.delay_for(1), Some(Duration::from_secs(86_400)));
        assert_eq!(policy.delay_for(2), Some(Duration::from_secs(3 * 86_400)));
        assert_eq!(policy.delay_for(3), Some(Duration::from_secs(5 * 86_400)));
        assert_eq!(policy.delay_for(4), None);
        assert_eq!(policy.delay_for(i64::MAX), None);

        let disabled = DunningPolicy::disabled();
        assert!(!disabled.enabled);
        assert_eq!(disabled.max_attempts(), 0);
        assert_eq!(disabled.delay_for(1), None);
        assert_eq!(disabled.on_exhausted, ExhaustionAction::MarkUnpaid);
    }

    #[test]
    fn exhaustion_action_serde_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&ExhaustionAction::CancelSubscription).unwrap(),
            r#""cancel_subscription""#
        );
        let back: ExhaustionAction = serde_json::from_str(r#""mark_unpaid""#).unwrap();
        assert_eq!(back, ExhaustionAction::MarkUnpaid);
    }
}
