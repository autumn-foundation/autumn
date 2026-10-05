//! H3: a report that the repository and Diesel's DSL cannot express.
//! H4: the report route carries `#[intercept(ReportGate)]`.

use autumn_web::prelude::*;
use autumn_web::reexports::diesel;
use autumn_web::reexports::diesel::sql_types::{BigInt, Text};
use autumn_web::reexports::diesel_async::RunQueryDsl;
use serde::Serialize;

use crate::hatches::report_gate::ReportGate;

/// One row of the stock-value report.
#[derive(Debug, Serialize, diesel::QueryableByName)]
pub struct StockValueRow {
    #[diesel(sql_type = Text)]
    pub category: String,
    #[diesel(sql_type = Text)]
    pub sku: String,
    #[diesel(sql_type = Text)]
    pub name: String,
    #[diesel(sql_type = BigInt)]
    pub value_cents: i64,
    #[diesel(sql_type = BigInt)]
    pub rank: i64,
}

/// Rank products by stock value in each category, and keep the top three.
const STOCK_VALUE_SQL: &str = "
    SELECT category, sku, name, value_cents, rank
    FROM (
        SELECT category, sku, name,
               stock::BIGINT * price_cents AS value_cents,
               ROW_NUMBER() OVER (
                   PARTITION BY category
                   ORDER BY stock::BIGINT * price_cents DESC, sku
               ) AS rank
        FROM products
    ) AS ranked
    WHERE rank <= 3
    ORDER BY category, rank";

/// The three most valuable products in each category, as JSON.
///
/// "Top N per group" needs a window function (`ROW_NUMBER() OVER
/// (PARTITION BY …)`). Repository finders and aggregates have none, and
/// Diesel's query DSL has none. So this is plain SQL through
/// `diesel::sql_query`, with typed rows from `QueryableByName`. The SQL has
/// no input, so it binds nothing.
///
/// The query reads every product. H4 (`ReportGate`) lets one run at a time.
#[get("/reports/stock-value")]
#[public]
#[intercept(ReportGate)]
pub async fn stock_value(mut db: Db) -> AutumnResult<Json<Vec<StockValueRow>>> {
    let rows = diesel::sql_query(STOCK_VALUE_SQL)
        .load::<StockValueRow>(&mut *db)
        .await?;
    Ok(Json(rows))
}
