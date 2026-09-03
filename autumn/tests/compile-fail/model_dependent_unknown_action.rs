//! #1702: an unrecognised `dependent = <action>` on a `#[has_many]` is a
//! directed compile error naming the four supported actions, not a confusing
//! generic parse failure (and never a silently-ignored key that would leave the
//! author believing a cascade is wired when none is).
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
    }
}

diesel::table! {
    comments (id) {
        id -> BigInt,
        body -> Text,
        post_id -> BigInt,
    }
}

#[model]
pub struct Comment {
    #[id]
    pub id: i64,
    pub body: String,
    pub post_id: i64,
}

#[model]
#[has_many(Comment, dependent = cascade)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
}

fn main() {}
