//! #1325: `counter_cache` is a **`belongs_to`** option. The counter is
//! maintained by the child's repository (the side that owns the foreign key and
//! runs the insert/delete), so declaring it on the parent's `#[has_many]` would
//! have nothing to hang the maintenance off. It is a directed compile error
//! that names the leg to move it to, not a silently-ignored key.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        comment_count -> BigInt,
    }
}

diesel::table! {
    comments (id) {
        id -> BigInt,
        body -> Text,
        post_id -> BigInt,
    }
}

#[model]
pub struct Comment {
    #[id]
    pub id: i64,
    pub body: String,
    pub post_id: i64,
}

#[model]
#[has_many(Comment, counter_cache)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub comment_count: i64,
}

fn main() {}
