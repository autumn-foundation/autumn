//! #1367: `author_id` is `i64` across the whole comments surface, and
//! `session_author` parses the session value with `str::parse::<i64>`. An
//! author model keyed by anything else used to compile and then return 401
//! from every authenticated POST, so the mismatch is caught here instead.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        comment_count -> BigInt,
    }
}

/// A UUID-keyed author, kept manual the way an author model usually is.
pub struct User {
    pub id: [u8; 16],
    pub username: String,
}

#[model]
#[commentable(by = User, author_name = username)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub comment_count: i64,
}

fn main() {}
