// Compile-fail: a schema name must be a plain lower-case identifier.
use autumn_web::model;

#[model(schema = "a.b")]
pub struct Widget {
    #[id]
    pub id: i64,
    pub name: String,
}

fn main() {}
