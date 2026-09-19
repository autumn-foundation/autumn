//! #1367: at most one `#[commentable]` per model. The emitted trait is
//! `{Model}Comments` with `add_comment`/`comment_thread`/`delete_comment`, so a
//! second declaration would generate colliding methods; reject it at macro time.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        comment_count -> BigInt,
    }
}

#[model]
#[commentable(by = User)]
#[commentable(by = User, table = notes)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub comment_count: i64,
}

fn main() {}
