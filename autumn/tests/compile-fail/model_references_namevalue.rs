// Compile-fail: `#[references]` is either a bare marker or the list form
// `#[references(table = "...")]`; a name-value `#[references = "..."]` shape is
// rejected with a clear message rather than a generic parser error (#1975,
// slice 3.5).
use autumn_web::model;

#[model]
pub struct Widget {
    #[id]
    pub id: i64,
    #[references = "accounts"]
    pub account_id: i64,
}

fn main() {}
