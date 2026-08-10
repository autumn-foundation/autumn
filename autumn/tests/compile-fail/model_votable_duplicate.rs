//! #1362: at most one `#[votable]` per model. The emitted trait is
//! `{Model}Reactions` with `react`/`reaction_of`, so a second declaration
//! would generate colliding methods; reject it at macro time instead.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        score -> BigInt,
        like_count -> BigInt,
    }
}

#[model]
#[votable(by = User, aggregate = sum)]
#[votable(by = User, aggregate = count, name = like)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub score: i64,
    pub like_count: i64,
}

fn main() {}
