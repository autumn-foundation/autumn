//! Shared test helpers.

#![allow(dead_code)]

use autumn_web::test::TestDb;
use escape_hatches::MIGRATIONS;
use escape_hatches::models::NewProduct;
use escape_hatches::repositories::{PgProductRepository, ProductRepository};

/// The token that the scanner tests send.
pub const SCANNER_TOKEN: &str = "scanner-test-token";

/// One test at a time may use the shared tables.
static TABLES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Apply the real migration, empty the tables, and return the shared database.
///
/// Keep the guard until the test ends. It stops a parallel test from
/// emptying the tables under this one.
pub async fn fresh_db() -> (&'static TestDb, tokio::sync::MutexGuard<'static, ()>) {
    let guard = TABLES.lock().await;
    let db = TestDb::shared().await;
    let url = db.url().to_owned();
    tokio::task::spawn_blocking(move || autumn_web::migrate::run_pending(&url, MIGRATIONS))
        .await
        .expect("migration task")
        .expect("apply the stockroom migration");
    db.execute_sql("TRUNCATE order_lines, orders, products RESTART IDENTITY CASCADE")
        .await;
    (db, guard)
}

/// A product row for a test.
pub fn product(sku: &str, category: &str, stock: i32, price_cents: i64) -> NewProduct {
    NewProduct {
        sku: sku.to_owned(),
        name: format!("Product {sku}"),
        category: category.to_owned(),
        stock,
        price_cents,
    }
}

/// Insert products through the repository (the convention).
pub async fn seed(db: &TestDb, rows: &[NewProduct]) {
    PgProductRepository::with_pool_untracked(db.pool())
        .save_many(rows)
        .await
        .expect("seed products");
}

/// Read the stock of one SKU through the repository.
pub async fn stock_of(db: &TestDb, sku: &str) -> i32 {
    PgProductRepository::with_pool_untracked(db.pool())
        .find_by_sku(sku.to_owned())
        .await
        .expect("find product")
        .first()
        .unwrap_or_else(|| panic!("no product {sku}"))
        .stock
}
