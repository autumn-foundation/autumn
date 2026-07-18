// Compile-fail: an unknown `#[model(...)]` argument is rejected with a clear
// message (#1975, slice 3.5).
use autumn_web::model;

#[model(bogus)]
pub struct Widget {
    #[id]
    pub id: i64,
    pub name: String,
}

fn main() {}
