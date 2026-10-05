//! H9: package the supplier router as a plugin.

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;

use crate::supplier::Catalog;

/// Mounts the supplier router under `/supplier`.
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
}

impl Plugin for SupplierPlugin {
    fn build(self, _app: AppBuilder) -> AppBuilder {
        todo!("H9")
    }
}
