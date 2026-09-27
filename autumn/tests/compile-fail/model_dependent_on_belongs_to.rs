//! #1702: `dependent`/`on_delete` is a **`has_many`/`has_one`** option. On a
//! `#[belongs_to]` the foreign key lives on *this* side, so there is no
//! dependent child set to cascade into — the key would silently do nothing.
//! It is a directed compile error that names the leg to move it to.
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
#[belongs_to(Post, dependent = destroy)]
pub struct Comment {
    #[id]
    pub id: i64,
    pub body: String,
    pub post_id: i64,
}

fn main() {}
