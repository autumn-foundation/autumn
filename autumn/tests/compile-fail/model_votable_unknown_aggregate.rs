//! #1362: only `sum` (signed up/down votes) and `count` (unary likes) are
//! supported aggregates — average/star ratings are explicitly out of scope, so
//! an unknown mode must not silently fall back to a default.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        score -> BigInt,
    }
}

#[model]
#[votable(by = User, aggregate = avg)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub score: i64,
}

fn main() {}
