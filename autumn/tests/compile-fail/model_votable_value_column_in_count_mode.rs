//! #1362: a `aggregate = count` edge table stores pure membership rows with no
//! value column, so `value_column = ...` is a category error rather than a
//! harmless extra key.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        vote_count -> BigInt,
    }
}

#[model]
#[votable(by = User, aggregate = count, value_column = value)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub vote_count: i64,
}

fn main() {}
