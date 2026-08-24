//! #1367: the counter is moved with `SET c = c + 1` and read back as `i64`, so
//! anything narrower is a schema mismatch rather than a convenience to coerce.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        comment_count -> Int4,
    }
}

#[model]
#[commentable(by = User)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub comment_count: i32,
}

fn main() {}
