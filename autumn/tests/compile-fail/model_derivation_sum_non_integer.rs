//! #1769: `transform = sum(<field>)` adds the field into an integer column, so
//! the field has to be a non-nullable integer. A nullable or non-numeric sum
//! would make the Rust and SQL lowerings of one derivation disagree.
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
#[derivation(Post, column = "published_comment_count", transform = sum(status))]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub published: bool,
    pub status: String,
}

fn main() {}
