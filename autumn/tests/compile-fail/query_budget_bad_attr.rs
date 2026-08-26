//! A malformed budget is a typo, not an opt-out: `#[query_budget]` rejects an
//! unknown keyword instead of silently declining to check the handler.

use autumn_web::query_budget;

#[query_budget(infinite)]
async fn show() -> usize {
    0
}

fn main() {}
