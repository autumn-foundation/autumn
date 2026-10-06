//! The models. This is the convention: one `#[model]` struct per table.
//!
//! `#[model]` makes `Product`, `NewProduct` (no `#[id]`), and `UpdateProduct`
//! (each field is a `Patch<T>`).

use serde::{Deserialize, Serialize};

use crate::schema::{order_lines, orders, products};

/// A product on a shelf.
#[autumn_web::model]
pub struct Product {
    #[id]
    pub id: i64,
    #[indexed]
    pub sku: String,
    pub name: String,
    #[indexed]
    pub category: String,
    pub stock: i32,
    pub price_cents: i64,
}

/// One checkout. `order_ref` is unique, so a retried checkout cannot apply twice.
#[autumn_web::model]
pub struct Order {
    #[id]
    pub id: i64,
    #[indexed]
    pub order_ref: String,
}

/// One line of an order.
#[autumn_web::model]
pub struct OrderLine {
    #[id]
    pub id: i64,
    #[indexed]
    pub order_id: i64,
    pub product_id: i64,
    pub sku: String,
    pub quantity: i32,
}

/// The checkout request body: a cart.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Cart {
    pub order_ref: String,
    pub lines: Vec<CartLine>,
}

/// One cart line.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CartLine {
    pub sku: String,
    pub quantity: i32,
}

/// The order that a checkout makes.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Receipt {
    pub order_ref: String,
    pub lines: Vec<CartLine>,
}
