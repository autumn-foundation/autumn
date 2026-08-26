//! #1362: `#[votable]` has no positional head — the reactor model is always
//! named with `by = <Model>`, so omitting it is a directed compile error
//! rather than an inferred guess.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        score -> BigInt,
    }
}

#[model]
#[votable(aggregate = sum)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub score: i64,
}

fn main() {}
