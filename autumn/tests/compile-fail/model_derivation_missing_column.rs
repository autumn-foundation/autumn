//! #1769: a derivation with no `column` names no output, so there is nothing
//! for it to maintain. Required rather than conventional: unlike a counter
//! cache there is no child-derived default that could be right.
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
#[derivation(Post)]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub published: bool,
    pub status: String,
}

fn main() {}
