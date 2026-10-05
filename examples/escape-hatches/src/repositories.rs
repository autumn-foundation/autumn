//! The repositories. This is the convention for reads and simple writes.
//!
//! `#[repository]` makes `PgProductRepository` and its siblings: CRUD, bulk
//! writes, `with_lock`, and one finder per method below.

use crate::models::{NewOrder, NewOrderLine, NewProduct, Order, OrderLine, Product};
use crate::models::{UpdateOrder, UpdateOrderLine, UpdateProduct};
use crate::schema::{order_lines, orders, products};

#[autumn_web::repository(Product)]
pub trait ProductRepository {
    fn find_by_sku(sku: String) -> Vec<Product>;
    fn find_by_category(category: String) -> Vec<Product>;
}

#[autumn_web::repository(Order)]
pub trait OrderRepository {
    fn find_by_order_ref(order_ref: String) -> Vec<Order>;
}

#[autumn_web::repository(OrderLine)]
pub trait OrderLineRepository {
    fn find_by_order_id(order_id: i64) -> Vec<OrderLine>;
}
