//! #1769: with two `#[belongs_to]` legs to one parent there is no ground to
//! prefer either foreign key, and guessing would count the wrong parent. The
//! derivation must name the key with `fk = <column>`.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        published_comment_count -> BigInt,
    }
}

diesel::table! {
    comments (id) {
        id -> BigInt,
        post_id -> BigInt,
        origin_id -> BigInt,
        published -> Bool,
    }
}

#[model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub published_comment_count: i64,
}

#[model]
#[belongs_to(Post, fk = post_id, name = post)]
#[belongs_to(Post, fk = origin_id, name = origin)]
#[derivation(Post, column = "published_comment_count", filter = published)]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub origin_id: i64,
    pub published: bool,
}

fn main() {}
