// Compile-fail: `#[renamed_from]` takes one snake_case name (#1975).
use autumn_web::model;

#[model(managed)]
pub struct Widget {
    #[id]
    pub id: i64,
    #[renamed_from("Old Slug")]
    pub slug: String,
}

fn main() {}
