//! #1769: a filter identifier becomes a column in the emitted SQL, so it must
//! name a real field of the child model. Rejecting it at macro time is what
//! keeps the grammar's identifiers safe to splice.
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
#[derivation(Post, column = "published_comment_count", filter = approved)]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub published: bool,
    pub status: String,
}

fn main() {}
