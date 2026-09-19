//! #1325: the counter column name is spliced verbatim into generated SQL
//! (`UPDATE posts SET <column> = <column> + $1 ...`), so anything that is not a
//! plain Rust/SQL identifier is rejected at macro time rather than reaching
//! `format!`.
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
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
}

#[model]
#[belongs_to(Post, counter_cache = "comment_count; DROP TABLE posts")]
pub struct Comment {
    #[id]
    pub id: i64,
    pub body: String,
    pub post_id: i64,
}

fn main() {}
