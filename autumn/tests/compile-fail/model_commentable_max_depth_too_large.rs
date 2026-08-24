//! #1367: the runtime measures nesting with a recursive CTE that stops at 1000,
//! so a `max_depth` at or above that could never be enforced — the probe would
//! report the guard rather than the real depth and the cap would silently stop
//! existing.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        comment_count -> BigInt,
    }
}

#[model]
#[commentable(by = User, max_depth = 5000)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub comment_count: i64,
}

fn main() {}
