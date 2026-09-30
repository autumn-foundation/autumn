//! #2662 compile-pass: a counter-cached `belongs_to` that names the parent key
//! column with `parent_pk`. The generated preload loader then filters on and
//! selects that column instead of `posts::id`, so this pins that the override
//! branch type-checks. (The parent keeps an `id` column because `#[model]`
//! itself still requires one; the child counts through the unique
//! `post_uuid` key.)
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        post_uuid -> BigInt,
        title -> Text,
        comment_count -> BigInt,
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
    pub post_uuid: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
}

#[model]
#[belongs_to(Post, counter_cache, parent_pk = "post_uuid")]
pub struct Comment {
    #[id]
    pub id: i64,
    pub body: String,
    pub post_id: i64,
}

fn main() {
    let specs = Comment::counter_caches();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].parent_table, "posts");
    assert_eq!(specs[0].parent_pk, "post_uuid");
    assert_eq!(specs[0].counter_column, "comment_count");
}
