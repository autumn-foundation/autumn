// Compile-fail: `#[references(...)]` accepts only `table`, `on_delete` and
// `on_update` (#1975).
use autumn_web::model;

#[model]
pub struct Widget {
    #[id]
    pub id: i64,
    #[references(bogus = "x")]
    pub account_id: i64,
}

fn main() {}
