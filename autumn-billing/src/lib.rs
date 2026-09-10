//! # autumn-billing
//!
//! Subscription billing for `autumn-web` applications: hosted checkout, the
//! customer portal, a webhook-fed local mirror of billing state, a plan gate,
//! and durable dunning. Stripe ships first behind a provider-neutral
//! [`BillingProvider`] trait.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use autumn_billing::prelude::*;
//!
//! let billing = BillingConfig::from_autumn_toml("autumn.toml")?;
//! let plans = PlanCatalog::new().plan(
//!     Plan::new("pro", "Pro", "price_123", Money::from_minor(1999, Currency::USD), BillingInterval::Month)
//!         .entitlement("export"),
//! );
//! autumn_web::app()
//!     .plugin(BillingPlugin::new().config(billing).plans(&plans))
//!     .run()
//!     .await;
//! ```
//!
//! Declare the webhook receiver in `autumn.toml` so `SignedWebhook` verifies
//! it and CSRF exempts it:
//!
//! ```toml
//! [[security.webhooks.endpoints]]
//! name = "billing"
//! path = "/billing/webhook"
//! provider = "stripe"
//! secret_env = "STRIPE_WEBHOOK_SECRET"
//! ```
//!
//! Gate a route:
//!
//! ```rust,ignore
//! struct Pro;
//! impl PlanRequirement for Pro {
//!     fn rule() -> PlanRule { PlanRule::plan("pro") }
//! }
//!
//! #[get("/reports")]
//! async fn reports(_pro: Entitled<Pro>) -> &'static str { "ok" }
//! ```

pub mod config;
pub mod dunning;
pub mod error;
pub mod event;
pub mod gate;
pub mod hooks;
pub mod model;
pub mod money;
pub mod notify;
pub mod plan;
pub mod provider;
pub mod reconcile;
pub mod routes;
pub mod store;
mod stripe;

pub use config::{BillingConfig, DunningPolicy, ExhaustionAction, SecretString, StripeConfig};
pub use error::BillingError;
pub use event::{
    BillingEvent, BillingEventKind, CheckoutSnapshot, InvoiceSnapshot, SubscriptionSnapshot,
};
pub use gate::{Billing, Entitled, PlanRequirement, PlanRule, SubscriptionView};
pub use hooks::{BillingHooks, NoHooks};
pub use model::{
    Customer, DunningAttempt, DunningState, Invoice, InvoiceStatus, ProviderId, Subscription,
    SubscriptionStatus,
};
pub use money::{Currency, Money, MoneyError};
pub use plan::{BillingInterval, Plan, PlanCatalog, PlanId};
pub use provider::{
    BillingProvider, CheckoutRequest, CustomerRequest, HostedSession, PaymentAttemptOutcome,
    PortalRequest,
};
pub use reconcile::ReconcileOutcome;
#[cfg(feature = "db")]
pub use store::DbBillingStore;
pub use store::{BillingStore, MemoryBillingStore};
pub use stripe::StripeProvider;

/// Common imports for configuring and mounting the plugin.
pub mod prelude {
    pub use crate::{
        Billing, BillingConfig, BillingError, BillingHooks, BillingInterval, BillingPlugin,
        BillingProvider, BillingStore, Currency, DunningPolicy, Entitled, ExhaustionAction,
        MemoryBillingStore, Money, Plan, PlanCatalog, PlanId, PlanRequirement, PlanRule,
        Subscription, SubscriptionStatus, SubscriptionView,
    };
}

use std::borrow::Cow;
use std::sync::Arc;

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::plugin_contract::PluginContract;
use autumn_web::{AppState, AutumnError};

/// How long a ledger claim in `processing` blocks redelivery before it is
/// treated as abandoned.
pub const EVENT_CLAIM_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

/// Embedded migrations for the mirror tables. Registered by [`BillingPlugin`].
#[cfg(feature = "db")]
pub const MIGRATIONS: autumn_web::migrate::EmbeddedMigrations = {
    use autumn_web::reexports::diesel_migrations;
    autumn_web::migrate::embed_migrations!("migrations")
};

/// Runtime handle installed on `AppState` by the plugin.
pub struct BillingService {
    provider: Arc<dyn BillingProvider>,
    store: Arc<dyn BillingStore>,
    catalog: Arc<PlanCatalog>,
    config: Arc<BillingConfig>,
    hooks: Arc<dyn BillingHooks>,
}

impl std::fmt::Debug for BillingService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingService")
            .field("provider", &self.provider.name())
            .finish_non_exhaustive()
    }
}

impl BillingService {
    /// Build a service. Tests use this to wire a fake provider and a memory store.
    #[must_use]
    pub fn new(
        provider: Arc<dyn BillingProvider>,
        store: Arc<dyn BillingStore>,
        catalog: PlanCatalog,
        config: BillingConfig,
        hooks: Arc<dyn BillingHooks>,
    ) -> Self {
        Self {
            provider,
            store,
            catalog: Arc::new(catalog),
            config: Arc::new(config),
            hooks,
        }
    }

    /// The service installed on `state`, if the plugin started.
    #[must_use]
    pub fn from_state(state: &AppState) -> Option<Arc<Self>> {
        state.extension::<Self>()
    }

    /// The service, or a 503 error when the plugin did not start.
    ///
    /// # Errors
    ///
    /// Returns `AutumnError::service_unavailable` when the plugin did not start.
    pub fn require(state: &AppState) -> Result<Arc<Self>, AutumnError> {
        Self::from_state(state)
            .ok_or_else(|| AutumnError::service_unavailable_msg("billing plugin is not started"))
    }

    /// The provider.
    #[must_use]
    pub fn provider(&self) -> &Arc<dyn BillingProvider> {
        &self.provider
    }

    /// The mirror store.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn BillingStore> {
        &self.store
    }

    /// The plan catalog.
    #[must_use]
    pub fn catalog(&self) -> &PlanCatalog {
        &self.catalog
    }

    /// The config.
    #[must_use]
    pub fn config(&self) -> &BillingConfig {
        &self.config
    }

    /// The hooks.
    #[must_use]
    pub fn hooks(&self) -> &Arc<dyn BillingHooks> {
        &self.hooks
    }
}

/// The billing plugin. Mount with `app.plugin(BillingPlugin::new()...)`.
pub struct BillingPlugin {
    config: BillingConfig,
    catalog: PlanCatalog,
    provider: Option<Arc<dyn BillingProvider>>,
    store: Option<Arc<dyn BillingStore>>,
    hooks: Arc<dyn BillingHooks>,
}

impl std::fmt::Debug for BillingPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingPlugin")
            .field("config", &self.config)
            .field("plans", &self.catalog)
            .finish_non_exhaustive()
    }
}

impl Default for BillingPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl BillingPlugin {
    /// Defaults: Stripe from the environment, plans from config, store from the pool.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: BillingConfig::from_env(),
            catalog: PlanCatalog::new(),
            provider: None,
            store: None,
            hooks: Arc::new(NoHooks),
        }
    }

    /// Supply the resolved `[billing]` config.
    #[must_use]
    pub fn config(mut self, config: BillingConfig) -> Self {
        self.config = config;
        self
    }

    /// Supply plans declared in code. Merged with `[[billing.plans]]`.
    #[must_use]
    pub fn plans(mut self, catalog: &PlanCatalog) -> Self {
        for plan in catalog.plans() {
            self.catalog.insert(plan.clone());
        }
        self
    }

    /// Replace the provider (default: Stripe).
    #[must_use]
    pub fn provider(mut self, provider: Arc<dyn BillingProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Replace the store (default: the database when a pool exists, else memory).
    #[must_use]
    pub fn store(mut self, store: Arc<dyn BillingStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Install application hooks.
    #[must_use]
    pub fn hooks(mut self, hooks: Arc<dyn BillingHooks>) -> Self {
        self.hooks = hooks;
        self
    }

    fn resolve_store(
        state: &AppState,
        injected: Option<Arc<dyn BillingStore>>,
    ) -> Arc<dyn BillingStore> {
        if let Some(store) = injected {
            return store;
        }
        #[cfg(feature = "db")]
        if let Some(pool) = autumn_web::db::DbState::pool(state).cloned() {
            return Arc::new(DbBillingStore::new(pool));
        }
        if state.profile() != "test" {
            tracing::warn!(
                "🍂 Autumn Billing: no database pool; using the in-memory mirror, which is lost on restart"
            );
        }
        Arc::new(MemoryBillingStore::new())
    }

    fn resolve_provider(
        state: &AppState,
        config: &BillingConfig,
        injected: Option<Arc<dyn BillingProvider>>,
    ) -> Result<Arc<dyn BillingProvider>, BillingError> {
        if let Some(provider) = injected {
            return Ok(provider);
        }
        Ok(Arc::new(StripeProvider::from_state(state, &config.stripe)?))
    }
}

impl Plugin for BillingPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("autumn-billing")
    }

    fn contract(&self) -> Option<PluginContract> {
        Some(autumn_web::plugin_contract::lockstep_contract(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let Self {
            mut config,
            mut catalog,
            provider,
            store,
            hooks,
        } = self;
        for plan in config.plans.drain(..) {
            catalog.insert(plan);
        }

        let app = app.config_section("billing");
        let app = app
            .nest(&config.route_prefix, routes::router())
            .declare_plugin_routes(routes::route_infos(&config));
        let app = app.jobs(dunning::job_infos());
        #[cfg(feature = "db")]
        let app = app.plugin_migrations("autumn-billing", MIGRATIONS);

        app.on_startup(move |state| {
            let config = config.clone();
            let catalog = catalog.clone();
            let provider = provider.clone();
            let store = store.clone();
            let hooks = hooks.clone();
            async move {
                let is_production = matches!(state.profile(), "prod" | "production");
                config.validate(is_production)?;
                let provider = Self::resolve_provider(&state, &config, provider)?;
                let store = Self::resolve_store(&state, store);
                verify_webhook_endpoint(&state, &config, provider.as_ref())?;
                // `insert_extension` wraps the value in its own `Arc`, keyed
                // by `BillingService`; read that handle back for the re-arm.
                state.insert_extension(BillingService {
                    provider,
                    store,
                    catalog: Arc::new(catalog),
                    config: Arc::new(config),
                    hooks,
                });
                let service = BillingService::require(&state)?;
                dunning::rearm_pending(state.clone(), service);
                Ok(())
            }
        })
    }
}

/// Fail boot when the app did not declare the webhook receiver, or declared
/// it with another verification preset than `provider` needs.
fn verify_webhook_endpoint(
    state: &AppState,
    config: &BillingConfig,
    provider: &dyn BillingProvider,
) -> Result<(), AutumnError> {
    let path = config.webhook_path();
    let expected = provider
        .webhook_endpoint(&config.endpoint_name, &path)
        .map_err(BillingError::into_autumn)?;
    let preset = expected.provider.as_str();
    let config_arc = state.config_arc();
    let declared = config_arc
        .security
        .webhooks
        .endpoints
        .iter()
        .find(|endpoint| endpoint.path == path);
    match declared {
        Some(endpoint) if endpoint.provider == expected.provider => Ok(()),
        Some(endpoint) => Err(AutumnError::internal_server_error_msg(format!(
            "autumn-billing: the signed webhook endpoint at {path} declares provider = \"{}\", \
             but the {} billing provider needs provider = \"{preset}\". Fix the \
             [[security.webhooks.endpoints]] entry in autumn.toml.",
            endpoint.provider.as_str(),
            provider.name()
        ))),
        None => Err(AutumnError::internal_server_error_msg(format!(
            "autumn-billing: no signed webhook endpoint is declared at {path}. Add to autumn.toml:\n\
             [[security.webhooks.endpoints]]\n\
             name = \"{}\"\n\
             path = \"{path}\"\n\
             provider = \"{preset}\"\n\
             secret_env = \"{}_WEBHOOK_SECRET\"",
            config.endpoint_name,
            provider.name().to_uppercase()
        ))),
    }
}

// Route-conformance harness: the same checks `autumn-media-plugin`'s
// `conformance_tests` uses, so the plugin's naming, prefix and attribution
// conventions stay clean.
#[cfg(test)]
mod conformance_tests {
    use autumn_web::plugin::Plugin;
    use autumn_web::plugin_conformance::{
        CheckStatus, ConformanceConfig, check_collisions, check_duplicate_registration,
        check_route_attribution, check_route_prefix, check_sensitive_surfaces, run_conformance,
    };
    use autumn_web::route_listing::{RouteInfo, RouteSource};

    use super::{BillingConfig, BillingPlugin, routes};

    const PLUGIN_NAME: &str = "autumn-billing";
    const PREFIX: &str = "/billing";

    /// The routes `build` declares, attributed exactly as
    /// `declare_plugin_routes` attributes them at runtime.
    fn declared_routes() -> Vec<RouteInfo> {
        routes::route_infos(&BillingConfig::default())
    }

    #[test]
    fn build_declares_the_four_routes_under_the_prefix() {
        let routes = declared_routes();
        assert_eq!(routes.len(), 4, "the plugin declares exactly four routes");
        assert!(
            routes.iter().all(|route| route.path.starts_with(PREFIX)),
            "every declared route lives under the prefix"
        );
        assert!(
            routes
                .iter()
                .all(|route| route.source == RouteSource::Plugin(PLUGIN_NAME.to_owned())),
            "every declared route is attributed to the plugin"
        );
    }

    #[test]
    fn routes_are_attributed_to_plugin_name() {
        let result = check_route_attribution(PLUGIN_NAME, &declared_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "attribution failed: {}",
            result.message
        );
    }

    #[test]
    fn routes_live_under_prefix() {
        let result = check_route_prefix(PLUGIN_NAME, PREFIX, &[], &declared_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "prefix check failed: {}",
            result.message
        );
    }

    #[test]
    fn routes_have_no_collisions_in_isolation() {
        let (result, _) = check_collisions(&declared_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "unexpected collision: {}",
            result.message
        );
    }

    #[test]
    fn routes_have_no_undeclared_sensitive_surfaces() {
        let result = check_sensitive_surfaces(PLUGIN_NAME, &declared_routes(), &[]);
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "sensitive-surfaces check failed: {}",
            result.message
        );
    }

    #[test]
    fn single_registration_passes_duplicate_check() {
        let result = check_duplicate_registration(PLUGIN_NAME, &declared_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "single registration should pass: {}",
            result.message
        );
    }

    #[test]
    fn routes_pass_full_conformance() {
        let contract = BillingPlugin::new().contract().expect("a contract");
        let config = ConformanceConfig::new(PLUGIN_NAME)
            .contract(contract)
            .prefix(PREFIX);
        let report = run_conformance(&config, &declared_routes());
        assert!(
            report.passed(),
            "BillingPlugin conformance failed:\n{}",
            report.to_text_report()
        );
    }

    #[test]
    fn duplicate_registration_detected() {
        // Installing the plugin twice would double its routes.
        let mut routes = declared_routes();
        routes.extend(declared_routes());
        let result = check_duplicate_registration(PLUGIN_NAME, &routes);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "expected duplicate-registration FAIL when installed twice"
        );
    }

    #[test]
    fn collision_with_host_route_detected() {
        let mut routes = declared_routes();
        routes.push(RouteInfo {
            method: "POST".to_owned(),
            path: format!("{PREFIX}/checkout"),
            handler: "host::checkout".to_owned(),
            source: RouteSource::User,
            ..Default::default()
        });
        let (result, _) = check_collisions(&routes);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "expected collision to be detected"
        );
    }
}

#[cfg(test)]
mod contract_tests {
    use super::BillingPlugin;
    use autumn_web::plugin::Plugin;

    #[test]
    fn contract_declares_lockstep_with_own_crate() {
        let contract = BillingPlugin::new().contract().expect("a contract");
        assert_eq!(contract.plugin, env!("CARGO_PKG_NAME"));
        assert_eq!(
            contract.autumn_web.as_deref(),
            Some(autumn_web::plugin_contract::lockstep_range(env!("CARGO_PKG_VERSION")).as_str())
        );
        assert!(contract.experimental_surfaces.is_empty());
    }
}
