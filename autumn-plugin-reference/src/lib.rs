//! The pinned reference plugin — the compiled proof of Autumn's stable plugin
//! API surface (issue #1601).
//!
//! # Why this crate exists
//!
//! `autumn_web::plugin_contract::PLUGIN_SURFACES` *declares* which
//! plugin-facing APIs are stable. A declaration nothing compiles against is a
//! promise nobody checks, so this crate is the other half: a real
//! [`Plugin`] implementation that calls every
//! [`Stable`](autumn_web::plugin_contract::SurfaceTier::Stable) surface in the
//! registry, built by CI on every change to the framework.
//!
//! If a stable plugin-facing API is removed, renamed, or has its signature
//! changed, **this crate stops compiling** and the `plugin-contract` CI job
//! goes red — before the change reaches a plugin author's build.
//!
//! # How the two halves are kept in sync
//!
//! Every stable surface is exercised inside a block introduced by a
//! `// surface: <name>` marker. [`surface_coverage`](tests) scans this file's
//! own source for those markers and asserts the marker set is exactly the
//! registry's stable set — so a registry entry with no compiled call site fails
//! the test, and a call site with no registry entry does too.
//!
//! # What this crate is not
//!
//! It is not a useful plugin, and it is never published. It mounts a single
//! trivial route and installs deliberately inert subsystem providers. The
//! *shape* of each call is the deliverable, not its behaviour.

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
// surface: plugin_contract
use autumn_web::plugin_contract::PluginContract;
use autumn_web::prelude::*;
// `embed_migrations!` expands to unqualified `diesel_migrations::…` paths, so
// the crate has to be in scope. Autumn re-exports it precisely so a plugin does
// not need its own `diesel-migrations` dependency.
#[cfg(feature = "db")]
use autumn_web::reexports::diesel_migrations;

/// The URL prefix every route this plugin mounts lives under.
pub const PREFIX: &str = "/reference";

/// The `autumn-web` range this plugin declares.
///
/// Derived from the crate's own version rather than hard-coded, because the
/// reference plugin ships *inside* the framework repository and is by
/// construction always in lockstep with it. A real out-of-tree plugin either
/// writes a literal (`"0.7"`) or, if it too releases in lockstep, calls this
/// same helper.
#[must_use]
pub fn declared_autumn_web_range() -> String {
    autumn_web::plugin_contract::lockstep_range(env!("CARGO_PKG_VERSION"))
}

#[get("/reference")]
async fn index() -> &'static str {
    "autumn plugin reference"
}

/// A plugin that exercises every stable plugin-facing API.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReferencePlugin;

impl ReferencePlugin {
    /// Construct the reference plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Plugin for ReferencePlugin {
    // surface: Plugin::name
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("autumn-plugin-reference")
    }

    // surface: Plugin::contract
    fn contract(&self) -> Option<PluginContract> {
        Some(autumn_web::plugin_contract::lockstep_contract(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
    }

    // surface: Plugin::build
    fn build(self, app: AppBuilder) -> AppBuilder {
        // surface: AppBuilder::routes
        let app = app.routes(routes![index]);

        // surface: AppBuilder::on_startup
        let app = app.on_startup(|_state| async move {
            autumn_web::reexports::tracing::debug!("autumn-plugin-reference started");
            Ok(())
        });

        // surface: AppBuilder::on_shutdown
        let app = app.on_shutdown(|| async move {
            autumn_web::reexports::tracing::debug!("autumn-plugin-reference stopped");
        });

        // surface: AppBuilder::with_extension
        let app = app.with_extension(ReferenceMarker);

        // surface: AppBuilder::config_section
        let app = app.config_section("plugin_reference");

        // surface: AppBuilder::nest
        // surface: AppBuilder::declare_plugin_routes
        //
        // A raw router is opaque to the route listing, so the pair is what a
        // conformant plugin writes: mount it, then declare what it mounted.
        let app = app
            .nest(
                "/reference/raw",
                autumn_web::reexports::axum::Router::new().route(
                    "/ping",
                    autumn_web::reexports::axum::routing::get(|| async { "pong" }),
                ),
            )
            .declare_plugin_routes(vec![autumn_web::route_listing::RouteInfo {
                // surface: route_listing::RouteInfo
                method: "GET".to_owned(),
                path: "/reference/raw/ping".to_owned(),
                handler: "autumn_plugin_reference::ping".to_owned(),
                source: autumn_web::route_listing::RouteSource::Plugin(
                    "autumn-plugin-reference".to_owned(),
                ),
                ..Default::default()
            }]);

        // surface: AppBuilder::merge
        let app = app.merge(autumn_web::reexports::axum::Router::new());

        // surface: AppBuilder::error_pages
        let app = app.error_pages(ReferenceErrorPages);

        // surface: AppBuilder::with_config_loader
        let app = app.with_config_loader(providers::ReferenceConfigLoader);

        // surface: AppBuilder::with_telemetry_provider
        let app = app.with_telemetry_provider(providers::ReferenceTelemetryProvider);

        // surface: AppBuilder::with_session_store
        let app = app.with_session_store(providers::ReferenceSessionStore);

        #[cfg(feature = "db")]
        // surface: AppBuilder::with_pool_provider
        let app = app.with_pool_provider(providers::ReferencePoolProvider);

        #[cfg(feature = "db")]
        // surface: AppBuilder::plugin_migrations
        let app = app.plugin_migrations("autumn-plugin-reference", MIGRATIONS);

        // surface: AppBuilder::plugin
        // surface: AppBuilder::plugins
        //
        // A cooperative plugin mounts plugins of its own. Both spellings are
        // plugin-facing surface, so both are exercised.
        let app = app.plugin(CompanionPlugin);
        let app = app.plugins((SecondCompanionPlugin,));

        // surface: AppBuilder::plugin_contracts
        //
        // A cooperative plugin can read what the plugins around it declared —
        // the same list the route dump emits for `autumn plugin-check`.
        debug_assert!(
            app.plugin_contracts()
                .iter()
                .any(|c| c.plugin == env!("CARGO_PKG_NAME")),
            "the reference plugin's own contract should be recorded by now"
        );

        app
    }
}

/// Embedded migrations owned by this plugin. The directory is empty on
/// purpose: `plugin_migrations` takes the handle, and the handle is what the
/// signature check needs.
#[cfg(feature = "db")]
pub const MIGRATIONS: autumn_web::migrate::EmbeddedMigrations =
    autumn_web::migrate::embed_migrations!("migrations");

/// Renders error pages. Formats nothing interesting: the seam is what is under
/// test, not the markup.
pub struct ReferenceErrorPages;

impl autumn_web::error_pages::ErrorPageRenderer for ReferenceErrorPages {
    fn render_error(&self, ctx: &autumn_web::error_pages::ErrorContext) -> Markup {
        html! { h1 { (ctx.status.as_u16()) } }
    }
}

/// A typed value published into application state via `with_extension`.
#[derive(Debug, Clone, Copy)]
pub struct ReferenceMarker;

/// A plugin the reference plugin mounts, to exercise `AppBuilder::plugin` from
/// inside a `build`.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompanionPlugin;

impl Plugin for CompanionPlugin {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("autumn-plugin-reference/companion")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app
    }
}

/// A second companion, mounted through the tuple-taking `plugins`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SecondCompanionPlugin;

impl Plugin for SecondCompanionPlugin {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("autumn-plugin-reference/companion-2")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app
    }
}

/// Inert implementations of the tier-1 subsystem seams a plugin may replace.
///
/// Each one exists so the corresponding `with_*` call site compiles. None of
/// them is meant to run: the reference app is never booted.
pub mod providers {
    use autumn_web::config::{AutumnConfig, ConfigError, ConfigLoader};
    use autumn_web::config::{LogConfig, TelemetryConfig};
    use autumn_web::session::{SessionStore, SessionStoreError};
    use autumn_web::telemetry::{TelemetryGuard, TelemetryInitError, TelemetryProvider};
    use std::collections::HashMap;

    /// Loads the framework defaults, unchanged.
    #[derive(Debug, Clone, Copy)]
    pub struct ReferenceConfigLoader;

    impl ConfigLoader for ReferenceConfigLoader {
        async fn load(&self) -> Result<AutumnConfig, ConfigError> {
            Ok(AutumnConfig::default())
        }
    }

    /// Installs no subscriber and holds no exporter.
    #[derive(Debug, Clone, Copy)]
    pub struct ReferenceTelemetryProvider;

    impl TelemetryProvider for ReferenceTelemetryProvider {
        fn init(
            &self,
            _log: &LogConfig,
            _telemetry: &TelemetryConfig,
            _profile: Option<&str>,
        ) -> Result<TelemetryGuard, TelemetryInitError> {
            Ok(TelemetryGuard::disabled())
        }
    }

    /// Stores nothing; every load misses.
    #[derive(Debug, Clone, Copy)]
    pub struct ReferenceSessionStore;

    impl SessionStore for ReferenceSessionStore {
        async fn load(
            &self,
            _id: &str,
        ) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
            Ok(None)
        }

        async fn save(
            &self,
            _id: &str,
            _data: HashMap<String, String>,
        ) -> Result<(), SessionStoreError> {
            Ok(())
        }

        async fn destroy(&self, _id: &str) -> Result<(), SessionStoreError> {
            Ok(())
        }
    }

    /// Runs the application without a database.
    #[cfg(feature = "db")]
    #[derive(Debug, Clone, Copy)]
    pub struct ReferencePoolProvider;

    #[cfg(feature = "db")]
    impl autumn_web::db::DatabasePoolProvider for ReferencePoolProvider {
        // surface: db::Pool
        async fn create_pool(
            &self,
            _config: &autumn_web::config::DatabaseConfig,
        ) -> Result<
            Option<autumn_web::db::Pool<autumn_web::db::RuntimeConnection>>,
            autumn_web::db::PoolError,
        > {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::plugin_contract::{
        AUTUMN_WEB_VERSION, ContractVerdict, PLUGIN_SURFACES, SurfaceTier, evaluate,
        stable_surface_names,
    };
    use std::collections::BTreeSet;

    /// This file's own source, scanned for `// surface: <name>` markers.
    const SOURCE: &str = include_str!("lib.rs");

    fn route(method: &str, path: &str, handler: &str) -> autumn_web::route_listing::RouteInfo {
        autumn_web::route_listing::RouteInfo {
            method: method.to_owned(),
            path: path.to_owned(),
            handler: handler.to_owned(),
            source: autumn_web::route_listing::RouteSource::Plugin(
                "autumn-plugin-reference".to_owned(),
            ),
            ..Default::default()
        }
    }

    fn markers() -> BTreeSet<String> {
        SOURCE
            .lines()
            .filter_map(|l| l.trim().strip_prefix("// surface: "))
            .map(|n| n.trim().to_owned())
            .collect()
    }

    /// The gate: the stable registry and this crate's compiled call sites are
    /// the same set.
    ///
    /// A registry entry with no marker means Autumn promises stability for an
    /// API nothing in CI compiles. A marker with no registry entry means this
    /// crate is guarding something the contract never declared.
    /// The marker scan is textual (`include_str!`), so it cannot see `#[cfg]`.
    /// Two stable surfaces sit behind the `db` feature; with that feature off,
    /// their markers would still be counted and the gate would claim coverage
    /// it does not have. Compiling this assertion only when `db` is on keeps
    /// the claim honest — and CI runs `--all-features`, so the gate always
    /// runs where it matters.
    #[cfg(feature = "db")]
    #[test]
    fn every_stable_surface_is_exercised_here() {
        let declared: BTreeSet<String> = stable_surface_names().map(ToOwned::to_owned).collect();
        let exercised = markers();

        let missing: Vec<&String> = declared.difference(&exercised).collect();
        assert!(
            missing.is_empty(),
            "these surfaces are declared STABLE but no call site in \
             autumn-plugin-reference exercises them: {missing:?}\n\
             Add a call in `ReferencePlugin::build` under a `// surface: <name>` marker, or \
             drop the entry from PLUGIN_SURFACES."
        );

        let extra: Vec<&String> = exercised.difference(&declared).collect();
        assert!(
            extra.is_empty(),
            "these call sites are marked as plugin surface but are not declared STABLE in \
             PLUGIN_SURFACES: {extra:?}"
        );
    }

    /// Experimental surface is deliberately *not* exercised here: this crate is
    /// the stable-surface gate, and pinning experimental API here would make
    /// every intended experimental change look like a regression.
    #[test]
    fn no_experimental_surface_is_pinned_by_the_reference_plugin() {
        let exercised = markers();
        for s in PLUGIN_SURFACES {
            if s.tier == SurfaceTier::Experimental {
                assert!(
                    !exercised.contains(s.name),
                    "`{}` is experimental; the reference plugin must not pin it",
                    s.name
                );
            }
        }
    }

    #[test]
    fn the_declared_range_admits_the_framework_it_is_built_against() {
        let contract = ReferencePlugin::new().contract().expect("a contract");
        assert_eq!(
            evaluate(&contract, AUTUMN_WEB_VERSION),
            ContractVerdict::Compatible,
            "the reference plugin ships in lockstep; it must accept its own framework"
        );
    }

    #[test]
    fn the_reference_plugin_declares_no_experimental_dependence() {
        let contract = ReferencePlugin::new().contract().expect("a contract");
        assert!(contract.experimental_surfaces.is_empty());
    }

    #[test]
    fn mounting_the_reference_plugin_records_its_contract() {
        let builder = autumn_web::app().plugin(ReferencePlugin::new());
        assert!(builder.has_plugin("autumn-plugin-reference"));
        assert!(builder.has_plugin("autumn-plugin-reference/companion"));
        assert!(builder.has_plugin("autumn-plugin-reference/companion-2"));

        let contracts = builder.plugin_contracts();
        assert_eq!(contracts.len(), 1, "only the reference plugin declares one");
        assert_eq!(contracts[0].plugin, "autumn-plugin-reference");
    }

    #[test]
    fn the_reference_plugin_passes_its_own_conformance_run() {
        // surface: plugin_conformance
        use autumn_web::plugin_conformance::{ConformanceConfig, run_conformance};

        // The routes this plugin contributes, as they appear in the manifest.
        // Built here rather than introspected: the mounted `nest` router is
        // opaque to the listing, which is exactly why `declare_plugin_routes`
        // exists — and this mirrors what `autumn plugin-check` sees.
        let routes = vec![
            route("GET", "/reference", "autumn_plugin_reference::index"),
            route(
                "GET",
                "/reference/raw/ping",
                "autumn_plugin_reference::ping",
            ),
        ];

        let config = ConformanceConfig::new("autumn-plugin-reference")
            .prefix(PREFIX)
            .contract(ReferencePlugin::new().contract().expect("a contract"));

        let report = run_conformance(&config, &routes);
        assert!(
            report.passed(),
            "the reference plugin must model what it documents:\n{}",
            report.to_text_report()
        );
    }

    #[test]
    fn declared_range_is_the_series_below_one_zero() {
        let range = declared_autumn_web_range();
        let major = AUTUMN_WEB_VERSION.split('.').next().unwrap_or("0");
        if major == "0" {
            assert_eq!(
                range.matches('.').count(),
                1,
                "expected `0.MINOR`, got {range}"
            );
        } else {
            assert_eq!(range, major);
        }
    }
}
