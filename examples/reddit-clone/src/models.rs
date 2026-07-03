use autumn_web::storage::Blob;

use crate::schema::{comments, posts, subreddits, tags, users, votes};

// Manual model -- password_hash should never be auto-exposed via API.

#[derive(Debug, Clone, diesel::Queryable, diesel::Selectable, serde::Serialize)]
#[diesel(table_name = users)]
pub struct User {
    pub id: i64,
    pub username: String,
    #[serde(skip)]
    pub password_hash: String,
    pub karma: i64,
    pub role: String,
    pub created_at: chrono::NaiveDateTime,
    /// Blob handle pointing into the configured `BlobStore`. Lifecycle
    /// (presence, deletion) is the application's job; the bytes are the
    /// store's. Demonstrates the `[storage]` feature's `#[model]`
    /// integration end-to-end.
    pub avatar: Option<Blob>,
}

// `User` is a hand-written model (so `password_hash` is never auto-exposed),
// but it is the target of `#[belongs_to(User, ...)]` on `Post`/`Comment`/
// `Subreddit`. Make it a leaf preload target so `post.author()` works.
autumn_web::impl_preloadable_leaf!(User);

#[derive(Debug, Clone, diesel::Insertable, serde::Deserialize)]
#[diesel(table_name = users)]
pub struct NewUser {
    pub username: String,
    pub password_hash: String,
}

#[autumn_web::model]
#[belongs_to(User, fk = creator_id)]
#[has_many(Post)]
pub struct Subreddit {
    #[id]
    pub id: i64,
    #[indexed]
    #[validate(length(min = 2, max = 32))]
    pub name: String,
    #[indexed]
    pub slug: String,
    pub description: String,
    pub creator_id: i64,
    #[default]
    pub subscriber_count: i64,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

#[autumn_web::model]
#[belongs_to(User, fk = author_id)]
#[belongs_to(Subreddit)]
#[has_many(Comment)]
#[has_many(Tag, through = post_tags)]
pub struct Post {
    #[id]
    pub id: i64,
    #[validate(length(min = 1, max = 300))]
    pub title: String,
    #[indexed]
    pub slug: String,
    pub body: String,
    pub url: Option<String>,
    #[indexed]
    pub author_id: i64,
    #[indexed]
    pub subreddit_id: i64,
    #[default]
    pub score: i64,
    #[default]
    pub hot_rank: f64,
    #[default]
    pub comment_count: i64,
    #[default]
    pub created_at: chrono::NaiveDateTime,
    #[default]
    pub updated_at: chrono::NaiveDateTime,
}

// Many-to-many (#1324): `#[has_many(Post, through = post_tags)]` is the
// mirror image of `Post`'s `#[has_many(Tag, through = post_tags)]` --
// mutual `through =` on the same join table, exercising the same Box'd
// nested-spec plumbing belongs_to/has_many mutual recursion already needs
// (`Post` belongs_to `Subreddit`, `Subreddit` has_many `Post`).
#[autumn_web::model]
#[has_many(Post, through = post_tags)]
pub struct Tag {
    #[id]
    pub id: i64,
    #[validate(length(min = 1, max = 40))]
    pub name: String,
    #[indexed]
    pub slug: String,
}

#[autumn_web::model]
#[belongs_to(User, fk = author_id)]
#[belongs_to(Post)]
pub struct Comment {
    #[id]
    pub id: i64,
    #[validate(length(min = 1))]
    pub body: String,
    #[indexed]
    pub author_id: i64,
    #[indexed]
    pub post_id: i64,
    pub parent_id: Option<i64>,
    #[default]
    pub score: i64,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

// Manual model -- complex constraints.

#[allow(dead_code)] // Used by generated API routes; not directly referenced in app code
#[derive(Debug, Clone, diesel::Queryable, diesel::Selectable, serde::Serialize)]
#[diesel(table_name = votes)]
pub struct Vote {
    pub id: i64,
    pub user_id: i64,
    pub post_id: Option<i64>,
    pub comment_id: Option<i64>,
    pub value: i16,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, diesel::Insertable)]
#[diesel(table_name = votes)]
pub struct NewVote {
    pub user_id: i64,
    pub post_id: Option<i64>,
    pub comment_id: Option<i64>,
    pub value: i16,
}
