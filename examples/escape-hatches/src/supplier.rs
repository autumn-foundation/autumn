//! The supplier catalog: a plain Axum router.
//!
//! The supplier team owns this code. Their own service runs it too, so it
//! uses Axum only, with no Autumn types. It keeps its own state (the
//! catalog) with `Router::with_state`. Autumn mounts it as it is; see
//! `hatches::supplier_plugin` (H9).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
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
        let item = |sku: &str, name: &str, unit_cost_cents, lead_time_days| Item {
            sku: sku.to_owned(),
            name: name.to_owned(),
            unit_cost_cents,
            lead_time_days,
        };
        Self(Arc::new(vec![
            item("HAM-1", "Claw hammer", 900, 2),
            item("PNT-RED", "Red paint, 1 L", 1_100, 5),
            item("ZZ-9", "Torque wrench", 4_200, 10),
        ]))
    }
}

/// The supplier router: `GET /items` and `GET /items/{sku}`.
pub fn router<S>(catalog: Catalog) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    axum::Router::new()
        .route("/items", get(list))
        .route("/items/{sku}", get(one))
        .with_state(catalog)
}

async fn list(State(catalog): State<Catalog>) -> Json<Vec<Item>> {
    Json(catalog.0.as_ref().clone())
}

async fn one(
    State(catalog): State<Catalog>,
    Path(sku): Path<String>,
) -> Result<Json<Item>, StatusCode> {
    catalog
        .0
        .iter()
        .find(|item| item.sku == sku)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}
