//! #3033 compile-pass: a `#[model]` whose primary key column is not `id`,
//! in both spellings: a renamed `#[id]` field, and a renamed column only.
//! Each owns a `has_many`, a `has_one` and a `belongs_to` association.
use autumn_web::model;

diesel::table! {
    posts (post_uuid) {
        post_uuid -> BigInt,
        title -> Text,
        comment_count -> BigInt,
    }
}

diesel::table! {
    comments (comment_uuid) {
        comment_uuid -> BigInt,
        body -> Text,
        post_id -> BigInt,
    }
}

diesel::table! {
    profiles (id) {
        id -> BigInt,
        bio -> Text,
        post_id -> BigInt,
    }
}

// Spelling 1: the `#[id]` field itself is renamed.
#[model]
#[has_many(Comment)]
#[has_one(Profile)]
pub struct Post {
    #[id]
    pub post_uuid: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
}

// Spelling 2: the field is `id`, only its column is renamed.
#[model]
#[belongs_to(Post, references = "post_uuid", counter_cache)]
pub struct Comment {
    #[id]
    #[diesel(column_name = comment_uuid)]
    pub id: i64,
    pub body: String,
    pub post_id: i64,
}

#[model]
#[belongs_to(Post, references = "post_uuid")]
pub struct Profile {
    #[id]
    pub id: i64,
    pub bio: String,
    pub post_id: i64,
}

fn main() {}
