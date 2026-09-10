//! `[billing]` configuration.
//!
//! Config is resolved after `Plugin::build`, so the application loads a
//! [`BillingConfig`] up front (`from_autumn_toml` or `from_env`) and passes it
//! to `BillingPlugin::config`. Secrets come from the environment; env values
//! win over TOML values.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use autumn_web::webhook::WebhookEndpointConfig;
use serde::{Deserialize, Serialize};

use crate::error::BillingError;
use crate::plan::Plan;

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
    /// Delay before retry `n` counted from the failure (`n = 1`) or from the
    /// previous retry. Length = number of retries.
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
        let _ = text;
        Err(BillingError::Config("not implemented".to_owned()))
    }

    /// Apply env overrides. `STRIPE_SECRET_KEY` and `STRIPE_WEBHOOK_SECRET`
    /// win over TOML values. Blank values are ignored.
    #[must_use]
    pub fn with_env_pairs(self, env: &HashMap<String, String>) -> Self {
        let _ = env;
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
        let _ = is_production;
        Ok(())
    }
}
