//! #1325 compile-pass: every shape of `counter_cache` the `#[model]` macro
//! accepts, so both branches of the emitter type-check.
//!
//! * `Comment` takes the bare flag — the column is convention-derived
//!   (`{snake(Child)}_count` -> `comment_count`) and the parent table is
//!   convention-derived from the target type (`Post` -> `posts`).
//! * `Membership` overrides the column name explicitly.
//! * `Revision` carries a `deleted_at` column, switching on the live-row
//!   predicate in the generated decrement/recompute SQL.
//! * `Plain` declares a `belongs_to` with **no** `counter_cache`, proving a
//!   model without one still resolves to the empty blanket impl.
use autumn_web::model;
use autumn_web::repository::AutumnCounterCaches as _;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        comment_count -> BigInt,
        revision_count -> BigInt,
    }
}

diesel::table! {
    teams (id) {
        id -> BigInt,
        name -> Text,
        member_count -> BigInt,
    }
}

diesel::table! {
    comments (id) {
        id -> BigInt,
        body -> Text,
        post_id -> BigInt,
    }
}

diesel::table! {
    memberships (id) {
        id -> BigInt,
        team_id -> BigInt,
    }
}

diesel::table! {
    revisions (id) {
        id -> BigInt,
        post_id -> BigInt,
        deleted_at -> Nullable<Timestamp>,
    }
}

diesel::table! {
    plains (id) {
        id -> BigInt,
        post_id -> BigInt,
    }
}

#[model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
    #[default]
    pub revision_count: i64,
}

#[model]
pub struct Team {
    #[id]
    pub id: i64,
    pub name: String,
    #[default]
    pub member_count: i64,
}

#[model]
#[belongs_to(Post, counter_cache)]
pub struct Comment {
    #[id]
    pub id: i64,
    pub body: String,
    pub post_id: i64,
}

#[model]
#[belongs_to(Team, counter_cache = "member_count")]
pub struct Membership {
    #[id]
    pub id: i64,
    pub team_id: i64,
}

#[model]
#[belongs_to(Post, counter_cache)]
pub struct Revision {
    #[id]
    pub id: i64,
    pub post_id: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[model]
#[belongs_to(Post)]
pub struct Plain {
    #[id]
    pub id: i64,
    pub post_id: i64,
}

fn main() {
    // Convention-derived column and parent table.
    let specs = Comment::counter_caches();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].counter_column, "comment_count");
    assert_eq!(specs[0].parent_table, "posts");
    assert_eq!(specs[0].fk_column, "post_id");
    assert_eq!(specs[0].child_table, "comments");
    assert!(!specs[0].child_soft_delete);
    assert!(Comment::HAS_COUNTER_CACHES);

    // Explicit column override.
    let specs = Membership::counter_caches();
    assert_eq!(specs[0].counter_column, "member_count");
    assert_eq!(specs[0].parent_table, "teams");
    assert_eq!((specs[0].fk_of)(&Membership { id: 1, team_id: 7 }), Some(7));
    assert_eq!((specs[0].pk_of)(&Membership { id: 3, team_id: 7 }), 3);

    // A `deleted_at` column flips the live-row predicate on.
    assert!(Revision::counter_caches()[0].child_soft_delete);

    // No `counter_cache` -> the empty blanket impl.
    assert!(Plain::counter_caches().is_empty());
    assert!(!Plain::HAS_COUNTER_CACHES);
}
