//! #1769: a filter is lowered to SQL that names the column after the Rust
//! field, so a field renamed by `#[diesel(column_name = ...)]` would be
//! spliced under a name the table does not have. Rejected, the same call
//! `#[translatable]` makes for the same reason.
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
        is_live -> Bool,
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
#[derivation(Post, column = "published_comment_count", filter = published)]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    #[diesel(column_name = is_live)]
    pub published: bool,
}

fn main() {}
