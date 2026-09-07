//! #1769: `{c}` in the emitted filter SQL is the child-alias placeholder the
//! runtime substitutes, so a brace inside a filter string literal could forge
//! one. The second macro-time guard that carries the injection argument.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        featured_comment_count -> BigInt,
    }
}

diesel::table! {
    comments (id) {
        id -> BigInt,
        post_id -> BigInt,
        status -> Text,
    }
}

#[model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub featured_comment_count: i64,
}

#[model]
#[derivation(Post, column = "featured_comment_count", filter = status == "{c}.\"id\"")]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub status: String,
}

fn main() {}
