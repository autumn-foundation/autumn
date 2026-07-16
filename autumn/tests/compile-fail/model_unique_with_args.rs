// Compile-fail: `#[unique]` is a bare marker and takes no arguments (#1975).
use autumn_web::model;

#[model]
pub struct Widget {
    #[id]
    pub id: i64,
    #[unique(x)]
    pub slug: String,
}

fn main() {}
