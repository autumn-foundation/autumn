//! The scanner API.

use autumn_web::prelude::*;
use axum::response::Response;
use serde::Deserialize;

use crate::models::{Cart, Receipt};
use crate::repositories::PgProductRepository;

/// The body of a reserve call.
#[derive(Debug, Deserialize)]
pub struct Reserve {
    pub quantity: i32,
}

/// The body of a restock call.
#[derive(Debug, Deserialize)]
pub struct Restock {
    pub category: String,
    pub add: i32,
}

/// The convention for one product: `with_lock`.
#[post("/products/{sku}/reserve")]
pub async fn reserve(
    Path(_sku): Path<String>,
    _repo: PgProductRepository,
    _body: Json<Reserve>,
) -> AutumnResult<Json<serde_json::Value>> {
    todo!("convention")
}

/// H1 + H13: check out a cart.
#[post("/checkout")]
pub async fn checkout(_db: Db, _cart: Json<Cart>) -> AutumnResult<Response> {
    todo!("H1")
}

/// H2: add stock to a category.
#[post("/restock")]
pub async fn restock(_db: Db, _body: Json<Restock>) -> AutumnResult<Json<serde_json::Value>> {
    todo!("H2")
}

/// Read one order.
#[get("/orders/{order_ref}")]
pub async fn order(Path(_order_ref): Path<String>, _db: Db) -> AutumnResult<Json<Receipt>> {
    todo!("H13")
}
