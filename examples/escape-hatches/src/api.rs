//! The scanner API. `app()` mounts it under `/api` behind a bearer token (H5).
//!
//! - `reserve` is the convention for one product: `with_lock`.
//! - `checkout` (H1) and `restock` (H2) go below the repository, because the
//!   repository cannot do what they need. Each handler says why.
//! - `order` is the convention again: two repository reads.

use std::collections::BTreeMap;

use autumn_web::prelude::*;
use autumn_web::reexports::diesel::prelude::*;
use autumn_web::reexports::diesel::sql_types::Text;
use autumn_web::reexports::diesel_async::RunQueryDsl;
use autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
use axum::http::header::LOCATION;
use axum::response::Response;
use serde::Deserialize;

use crate::models::{Cart, CartLine, Receipt};
use crate::repositories::{
    OrderLineRepository, OrderRepository, PgOrderLineRepository, PgOrderRepository,
    PgProductRepository, product_by_sku,
};
use crate::schema::{order_lines, orders, products};

/// The most units that one call can move.
const MAX_QUANTITY: i32 = 10_000;

/// The most units of one product. The schema has the same CHECK.
pub const MAX_STOCK: i32 = 1_000_000;

/// The most distinct lines in one cart. It keeps one checkout transaction,
/// and the row locks that it holds, short.
const MAX_LINES: usize = 100;

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

/// Convention: reserve units of one product.
///
/// `with_lock` reads the row with `SELECT … FOR UPDATE` in a transaction.
/// A second caller waits, then reads the new stock. This is the framework's
/// pattern for inventory, and it is enough for one row.
#[post("/products/{sku}/reserve")]
pub async fn reserve(
    Path(sku): Path<String>,
    repo: PgProductRepository,
    Json(body): Json<Reserve>,
) -> AutumnResult<Json<serde_json::Value>> {
    let quantity = check_quantity(body.quantity)?;
    let product = product_by_sku(&repo, &sku).await?;

    let stock = repo
        .with_lock(product.id, move |row, conn| {
            async move {
                if row.stock < quantity {
                    return Err(AutumnError::conflict_msg(format!(
                        "not enough stock for {}",
                        row.sku
                    )));
                }
                let stock = row.stock - quantity;
                diesel::update(products::table.find(row.id))
                    .set(products::stock.eq(stock))
                    .execute(conn)
                    .await?;
                Ok(stock)
            }
            .scope_boxed()
        })
        .await?;
    Ok(Json(serde_json::json!({ "sku": sku, "stock": stock })))
}

/// H1 + H13: check out a cart. All lines, or none.
///
/// Why not the convention:
///
/// - `with_lock` locks one row in its own transaction. A cart needs all its
///   rows in one transaction, so that a short line (a line with not enough
///   stock) can roll back the lines before it.
/// - `#[lock_version]` finds a conflict, but the second caller gets 409 and
///   must retry. On a busy SKU, most callers fail.
///
/// So the handler opens `Db::tx` and sends one guarded `UPDATE` per line:
/// `SET stock = stock - n WHERE stock >= n`. Postgres does the check and the
/// write in one step. Lines run in SKU order. `restock` locks rows in SKU
/// order too, so no two calls wait for each other in a circle (a deadlock).
///
/// A retry with a used `order_ref` and the same lines gets the first receipt
/// with `200`, and sells nothing. So a scanner that lost a response can
/// retry. Other lines with a used `order_ref` get 409.
///
/// H13: there is no `Created` helper, so the handler returns a tuple:
/// `201`, a `Location` header, and the JSON body.
#[post("/checkout")]
pub async fn checkout(mut db: Db, Json(cart): Json<Cart>) -> AutumnResult<Response> {
    let lines = check_cart(&cart)?;
    let order_ref = cart.order_ref;
    let (status, receipt) = db
        .tx(|conn| {
            let order_ref = order_ref.clone();
            let lines = lines.clone();
            async move {
                let receipt = receipt_for(&order_ref, &lines);
                // A used `order_ref` inserts no row.
                let order_id: Option<i64> = diesel::insert_into(orders::table)
                    .values(orders::order_ref.eq(&order_ref))
                    .on_conflict_do_nothing()
                    .returning(orders::id)
                    .get_result(conn)
                    .await
                    .optional()?;
                let Some(order_id) = order_id else {
                    return replay(conn, receipt).await;
                };

                for (sku, quantity) in &lines {
                    let product_id: Option<i64> = diesel::update(
                        products::table
                            .filter(products::sku.eq(sku))
                            .filter(products::stock.ge(quantity)),
                    )
                    .set(products::stock.eq(products::stock - quantity))
                    .returning(products::id)
                    .get_result(conn)
                    .await
                    .optional()?;
                    let Some(product_id) = product_id else {
                        // An `Err` rolls back the whole transaction.
                        return Err(short_line_error(conn, sku).await);
                    };
                    diesel::insert_into(order_lines::table)
                        .values((
                            order_lines::order_id.eq(order_id),
                            order_lines::product_id.eq(product_id),
                            order_lines::sku.eq(sku),
                            order_lines::quantity.eq(quantity),
                        ))
                        .execute(conn)
                        .await?;
                }
                Ok::<_, AutumnError>((StatusCode::CREATED, receipt))
            }
            .scope_boxed()
        })
        .await?;

    let location = format!("/api/orders/{}", receipt.order_ref);
    Ok((status, [(LOCATION, location)], Json(receipt)).into_response())
}

/// The receipt for `lines`, in SKU order.
fn receipt_for(order_ref: &str, lines: &BTreeMap<String, i32>) -> Receipt {
    Receipt {
        order_ref: order_ref.to_owned(),
        lines: lines
            .iter()
            .map(|(sku, quantity)| CartLine {
                sku: sku.clone(),
                quantity: *quantity,
            })
            .collect(),
    }
}

/// The answer to a retry with a used `order_ref`: `200` and the stored
/// receipt if the lines are the same, else 409.
async fn replay(
    conn: &mut autumn_web::RuntimeConnection,
    requested: Receipt,
) -> AutumnResult<(StatusCode, Receipt)> {
    let mut stored: Vec<(String, i32)> = order_lines::table
        .inner_join(orders::table)
        .filter(orders::order_ref.eq(&requested.order_ref))
        .select((order_lines::sku, order_lines::quantity))
        .load(conn)
        .await?;
    // Sort in Rust (byte order), as the cart is. The database collation can
    // put SKUs in another order.
    stored.sort();
    let same = stored.len() == requested.lines.len()
        && stored
            .iter()
            .zip(&requested.lines)
            .all(|((sku, quantity), line)| *sku == line.sku && *quantity == line.quantity);
    if same {
        Ok((StatusCode::OK, requested))
    } else {
        Err(AutumnError::conflict_msg(format!(
            "order {} exists with other lines",
            requested.order_ref
        )))
    }
}

/// The error for a line that the guarded `UPDATE` did not change: the SKU is
/// missing (404) or short (409).
async fn short_line_error(conn: &mut autumn_web::RuntimeConnection, sku: &str) -> AutumnError {
    let exists = diesel::select(diesel::dsl::exists(
        products::table.filter(products::sku.eq(sku)),
    ))
    .get_result::<bool>(conn)
    .await;
    match exists {
        Ok(true) => AutumnError::conflict_msg(format!("not enough stock for {sku}")),
        Ok(false) => AutumnError::not_found_msg(format!("no product with SKU {sku}")),
        Err(error) => error.into(),
    }
}

/// H2: add stock to each product in a category.
///
/// Why not the convention: the repository writes absolute values. A loop of
/// `find_by_category` then `update` per row costs a round trip per product.
/// The loop also loses a checkout that runs between its read and its write
/// (see `hazard_repository_read_modify_write_loses_an_update`).
///
/// So the handler sends one relative `UPDATE … SET stock = stock + n`. It
/// writes no value that it read, so it cannot lose a checkout. First, in the
/// same transaction, it locks the rows in SKU order, the order that
/// `checkout` uses, so the two cannot deadlock. The lock also lets it refuse
/// a restock that passes [`MAX_STOCK`] before it changes any row.
#[post("/restock")]
pub async fn restock(
    mut db: Db,
    Json(body): Json<Restock>,
) -> AutumnResult<Json<serde_json::Value>> {
    let add = check_quantity(body.add)?;
    let category = body.category.trim().to_owned();
    if category.is_empty() {
        return Err(AutumnError::unprocessable_msg("category is empty"));
    }
    let updated = db
        .tx(|conn| {
            let category = category.clone();
            async move {
                let rows: Vec<(i64, String, i32)> = products::table
                    .filter(products::category.eq(&category))
                    // Byte order (`COLLATE "C"`), the order of the `BTreeMap`
                    // in `checkout`. The database collation can differ.
                    .order(diesel::dsl::sql::<Text>(r#"sku COLLATE "C""#))
                    .select((products::id, products::sku, products::stock))
                    .for_update()
                    .load(conn)
                    .await?;
                if rows.is_empty() {
                    return Err(AutumnError::not_found_msg(format!(
                        "no products in category {category}"
                    )));
                }
                if let Some((_, sku, _)) =
                    rows.iter().find(|(_, _, stock)| *stock > MAX_STOCK - add)
                {
                    return Err(AutumnError::conflict_msg(format!(
                        "restock would put {sku} over {MAX_STOCK} units"
                    )));
                }
                let ids: Vec<i64> = rows.iter().map(|(id, _, _)| *id).collect();
                let updated = diesel::update(products::table.filter(products::id.eq_any(ids)))
                    .set(products::stock.eq(products::stock + add))
                    .execute(conn)
                    .await?;
                Ok::<_, AutumnError>(updated)
            }
            .scope_boxed()
        })
        .await?;
    Ok(Json(
        serde_json::json!({ "category": category, "updated": updated }),
    ))
}

/// Convention: read one order through the repositories.
#[get("/orders/{order_ref}")]
pub async fn order(
    Path(order_ref): Path<String>,
    orders: PgOrderRepository,
    lines: PgOrderLineRepository,
) -> AutumnResult<Json<Receipt>> {
    let order = orders
        .find_by_order_ref(order_ref.clone())
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AutumnError::not_found_msg(format!("no order {order_ref}")))?;
    let mut lines = lines.find_by_order_id(order.id).await?;
    lines.sort_by(|a, b| a.sku.cmp(&b.sku));
    Ok(Json(Receipt {
        order_ref: order.order_ref,
        lines: lines
            .into_iter()
            .map(|line| CartLine {
                sku: line.sku,
                quantity: line.quantity,
            })
            .collect(),
    }))
}

/// Refuse a quantity outside `1..=MAX_QUANTITY`.
fn check_quantity(quantity: i32) -> AutumnResult<i32> {
    if (1..=MAX_QUANTITY).contains(&quantity) {
        Ok(quantity)
    } else {
        Err(AutumnError::unprocessable_msg(format!(
            "quantity must be 1 to {MAX_QUANTITY}"
        )))
    }
}

/// Check a cart. Merge lines for one SKU. Sort by SKU (the lock order).
fn check_cart(cart: &Cart) -> AutumnResult<BTreeMap<String, i32>> {
    let order_ref_ok = !cart.order_ref.is_empty()
        && cart.order_ref.len() <= 64
        && cart
            .order_ref
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !order_ref_ok {
        return Err(AutumnError::unprocessable_msg(
            "order_ref must be 1 to 64 letters, digits, '-' or '_'",
        ));
    }
    if cart.lines.is_empty() {
        return Err(AutumnError::unprocessable_msg("cart is empty"));
    }
    if cart.lines.len() > MAX_LINES {
        return Err(AutumnError::unprocessable_msg(format!(
            "a cart has at most {MAX_LINES} lines"
        )));
    }
    let mut lines = BTreeMap::new();
    for line in &cart.lines {
        let quantity = check_quantity(line.quantity)?;
        let total: &mut i32 = lines.entry(line.sku.clone()).or_default();
        *total = check_quantity(total.saturating_add(quantity))?;
    }
    Ok(lines)
}
