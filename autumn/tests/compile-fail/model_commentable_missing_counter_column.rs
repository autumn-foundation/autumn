//! #1367: `#[commentable]` maintains `{model}.comment_count` by default, so
//! the column has to exist. Without this check the mistake only surfaces as a
//! runtime `42703 column "comment_count" does not exist` on the first comment.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
    }
}

#[model]
#[commentable(by = User)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
}

fn main() {}
