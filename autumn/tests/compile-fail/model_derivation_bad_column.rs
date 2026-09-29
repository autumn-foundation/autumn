//! #1769: the maintained column name is spliced verbatim into generated SQL
//! (`UPDATE posts SET <column> = <column> + $1 ...`), so anything that is not
//! a plain identifier is rejected at macro time rather than reaching
//! `format!`. This is the guard that carries the injection argument for
//! `#[derivation]`, as `model_counter_cache_bad_column.rs` does for #1325.
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
        post_id -> BigInt,
        published -> Bool,
    }
}

#[model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
}

#[model]
#[derivation(Post, column = "published\"; DROP TABLE posts; --", filter = published)]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub published: bool,
}

fn main() {}
