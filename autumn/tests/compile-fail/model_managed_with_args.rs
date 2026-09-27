// Compile-fail: `#[model(managed)]` is a bare marker; a parenthesized
// `managed(...)` shape is rejected with a clear message rather than a generic
// parser error (#1975, slice 3.5).
use autumn_web::model;

#[model(managed(foo))]
pub struct Widget {
    #[id]
    pub id: i64,
    pub name: String,
}

fn main() {}
