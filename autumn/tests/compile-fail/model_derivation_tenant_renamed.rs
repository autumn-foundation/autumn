//! #1769: a derivation's `tenant = "<column>"` is spliced into SQL as the
//! physical column of both the child and the parent table, spelled after the
//! Rust field. A field renamed with `#[diesel(column_name = "...")]` would
//! scope every statement by a column the tables do not have — and naming the
//! database column instead fails the field lookup — so the rename is rejected.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        org_id -> BigInt,
        published_comment_count -> BigInt,
    }
}

diesel::table! {
    comments (id) {
        id -> BigInt,
        post_id -> BigInt,
        org_id -> BigInt,
        published -> Bool,
    }
}

#[model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub org_id: i64,
    #[default]
    pub published_comment_count: i64,
}

#[model]
#[derivation(Post, column = "published_comment_count", filter = published, tenant = "tenant")]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    #[diesel(column_name = "org_id")]
    pub tenant: i64,
    pub published: bool,
}

fn main() {}
