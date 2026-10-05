//! The supplier catalog router. Plain Axum: no Autumn types.

use std::sync::Arc;

use serde::Serialize;

/// One supplier item.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub sku: String,
    pub name: String,
    pub unit_cost_cents: i64,
    pub lead_time_days: u32,
}

/// The supplier catalog.
#[derive(Debug, Clone)]
pub struct Catalog(pub Arc<Vec<Item>>);

impl Catalog {
    /// A small fixed catalog.
    #[must_use]
    pub fn sample() -> Self {
        todo!("H9")
    }
}

/// The supplier router.
pub fn router<S>(_catalog: Catalog) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    todo!("H9")
}
