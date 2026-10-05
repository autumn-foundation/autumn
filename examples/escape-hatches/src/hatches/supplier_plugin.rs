//! H9: mount the supplier's plain Axum router as a plugin.
//!
//! The router has its own state, so it cannot become `#[get]` handlers
//! without a rewrite, and a rewrite forks the supplier's code. A `Plugin`
//! mounts it as it is:
//!
//! - `nest` puts it under `/supplier`.
//! - `declare_plugin_routes` lists its routes, so `autumn routes` and
//!   `autumn routes audit` can see them.
//! - Another Autumn app can mount the same catalog with one line.

use std::borrow::Cow;

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::route_listing::{RouteClassification, RouteInfo};

use crate::supplier::{self, Catalog};

/// Where the supplier router is mounted.
pub const PREFIX: &str = "/supplier";

/// Mounts the supplier router under [`PREFIX`].
pub struct SupplierPlugin {
    pub catalog: Catalog,
}

impl SupplierPlugin {
    /// The plugin with the sample catalog.
    #[must_use]
    pub fn sample() -> Self {
        Self {
            catalog: Catalog::sample(),
        }
    }

    /// The routes that the router serves, for the route listing.
    #[must_use]
    pub fn routes() -> Vec<RouteInfo> {
        ["/items", "/items/{sku}"]
            .into_iter()
            .map(|path| RouteInfo {
                method: "GET".to_owned(),
                path: format!("{PREFIX}{path}"),
                handler: "supplier::router".to_owned(),
                classification: RouteClassification::Public,
                ..Default::default()
            })
            .collect()
    }
}

impl Plugin for SupplierPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("stockroom-supplier")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app.nest(PREFIX, supplier::router(self.catalog))
            .declare_plugin_routes(Self::routes())
    }
}
