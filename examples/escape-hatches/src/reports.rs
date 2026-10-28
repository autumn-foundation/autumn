//! H3: a report that the repository and Diesel's DSL cannot express.
//! H4: the report route carries `#[intercept(ReportGate)]`.

use autumn_web::prelude::*;
use autumn_web::reexports::diesel;
use autumn_web::reexports::diesel::sql_types::{BigInt, Text};
use autumn_web::reexports::diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};

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

/// Rank products by stock value in each category, and keep the top `$1`.
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
    WHERE rank <= $1
    ORDER BY category, rank";

/// The query string of the report.
#[derive(Debug, Deserialize)]
pub struct ReportQuery {
    /// Products to keep in each category: 1 to 10. The default is 3.
    pub top: Option<i64>,
}

/// The `top` products by stock value in each category, as JSON.
///
/// "Top N per group" needs a window function: `ROW_NUMBER() OVER
/// (PARTITION BY …)`. Repository finders and aggregates have none. Diesel's
/// DSL can put `row_number()` in a SELECT, but it cannot filter on it. That
/// filter needs a subquery in FROM, or a CTE, and the DSL has neither. To
/// rank in the DSL and drop rows in Rust sends every product to the app.
///
/// So this is plain SQL through `diesel::sql_query`, with typed rows from
/// `QueryableByName`. The only input, `top`, goes in as a bound parameter
/// (`$1`), never as text in the SQL.
///
/// The query reads every product. H4 (`ReportGate`) lets one run at a time.
#[get("/reports/stock-value")]
#[public]
#[intercept(ReportGate)]
pub async fn stock_value(
    Query(query): Query<ReportQuery>,
    mut db: Db,
) -> AutumnResult<Json<Vec<StockValueRow>>> {
    let top = query.top.unwrap_or(3);
    if !(1..=10).contains(&top) {
        return Err(AutumnError::unprocessable_msg("top must be 1 to 10"));
    }
    let rows = diesel::sql_query(STOCK_VALUE_SQL)
        .bind::<BigInt, _>(top)
        .load::<StockValueRow>(&mut *db)
        .await?;
    Ok(Json(rows))
}
