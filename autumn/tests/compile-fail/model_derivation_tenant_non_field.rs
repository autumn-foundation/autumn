//! #1769: a derivation's `tenant = "<column>"` scopes the maintenance by a
//! column of the CHILD (`comments.tenant_id`), so it must name a field of the
//! child model. A missing one would fail as a bare SQL error at run time.
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
#[derivation(Post, column = "published_comment_count", filter = published, tenant = "tenant_id")]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub published: bool,
}

fn main() {}
