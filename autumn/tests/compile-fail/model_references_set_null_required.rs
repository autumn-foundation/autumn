// Compile-fail: `on_delete = "set_null"` needs an `Option<_>` field (#1975).
use autumn_web::model;

#[model]
pub struct Comment {
    #[id]
    pub id: i64,
    #[references(table = "posts", on_delete = "set_null")]
    pub post_id: i64,
}

fn main() {}
