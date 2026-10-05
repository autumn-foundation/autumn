//! H3: a report that the repository cannot express.

use autumn_web::prelude::*;

/// Top three products by stock value in each category.
#[get("/reports/stock-value")]
#[public]
pub async fn stock_value(_db: Db) -> AutumnResult<Json<serde_json::Value>> {
    todo!("H3")
}
