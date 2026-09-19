//! #1769: `transform = sum(<field>)` takes exactly one field name. An
//! expression inside the parentheses must not be silently read as its first
//! identifier, or the maintained aggregate would differ from what the source
//! says.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        visible_score -> BigInt,
    }
}

diesel::table! {
    comments (id) {
        id -> BigInt,
        post_id -> BigInt,
        score -> BigInt,
        bonus -> BigInt,
    }
}

#[model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub visible_score: i64,
}

#[model]
#[derivation(Post, column = "visible_score", transform = sum(score + bonus))]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub score: i64,
    pub bonus: i64,
}

fn main() {}
