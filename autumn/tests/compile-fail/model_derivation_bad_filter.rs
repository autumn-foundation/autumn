//! #1769: a derivation filter is lowered to BOTH a Rust predicate and a SQL
//! predicate, so the grammar admits only the productions that provably lower
//! the same way in both. Anything else is rejected with the grammar listed,
//! rather than silently dropped.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        published_comment_count -> BigInt,
    }
}

diesel::table! {
    comments (id) {
        id -> BigInt,
        post_id -> BigInt,
        published -> Bool,
        status -> Text,
    }
}

#[model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub published_comment_count: i64,
}

#[model]
#[derivation(Post, column = "published_comment_count", filter = published || status == "draft")]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub published: bool,
    pub status: String,
}

fn main() {}
