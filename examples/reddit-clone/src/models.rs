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

// Votable (#1362): `#[votable(by = User, aggregate = sum)]` is the whole
// upvote/downvote feature. Every default already matches the schema this
// example has shipped since its first migration -- table `votes`, columns
// `user_id` / `post_id` / `value`, aggregate column `posts.score`, and the
// load-bearing `UNIQUE (user_id, post_id)` the generated upsert names as its
// `ON CONFLICT` arbiter -- so there are no overrides and **no migration**.
// It emits a `PostReactions` trait (`react` / `reaction_of`) that
// `PgPostRepository` picks up; see `crate::routes::votes`.
#[autumn_web::model]
#[belongs_to(User, fk = author_id)]
#[belongs_to(Subreddit)]
#[has_many(Comment)]
#[has_many(Tag, through = post_tags)]
#[votable(by = User, aggregate = sum)]
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

// Counter cache (#1325): `counter_cache` on the `Post` leg is the whole
// `posts.comment_count` feature. The column already existed (this example has
// shipped it since its first migration, and the default column convention is
// `{snake(child)}_count` -> `comment_count`), so adding the option required
// **no migration**. It retires the hand-written
// `posts::comment_count.eq(posts::comment_count + 1)` that used to live in
// `crate::routes::comments` -- along with the decrement nobody had written yet.
#[autumn_web::model]
#[belongs_to(User, fk = author_id)]
#[belongs_to(Post, counter_cache)]
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

// `Vote` is an `#[autumn_web::model]` so `VoteRepository` (see
// `crate::repositories`) can expose the typed grouped-aggregate roll-up
// `sum_value_grouped_by_post_id` (#1364), which the front-page "Top posts by
// votes" leaderboard uses as a replica-eligible top-N read.
// A vote references *either* a post or a comment, so both FKs are nullable.
//
// This model **coexists with** `Post`'s `#[votable]` above rather than being
// replaced by it -- the two describe the same physical `votes` table from two
// angles, deliberately:
//
//   * `#[votable]` emits its own edge `diesel::table!` inside a *hidden*
//     private module (`__autumn_votable_..._post_vote`), declaring only the
//     three columns the write path touches and declaring `post_id` as
//     **non-nullable** `Int8`. That is what makes `ON CONFLICT (user_id,
//     post_id)` well-typed and makes a NULL target unrepresentable on the
//     write path. It never collides with, or redefines, `crate::schema::votes`.
//   * `Vote` maps the *full* `crate::schema::votes` -- including the nullable
//     `post_id` / `comment_id` XOR pair -- which is exactly what the
//     leaderboard's `IS NOT NULL` group guard needs to exclude comment votes.
//
// So: reads and roll-ups go through `Vote`/`VoteRepository`; writes go through
// `posts.react(...)`. Guarded by `leaderboard_grouped_aggregate_still_works_
// after_react` in `tests/votable_pg_integration.rs`.
#[autumn_web::model(table = "votes")]
pub struct Vote {
    #[id]
    pub id: i64,
    #[indexed]
    pub user_id: i64,
    #[indexed]
    pub post_id: Option<i64>,
    #[indexed]
    pub comment_id: Option<i64>,
    pub value: i16,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}
