//! #1362 compile-pass: `#[votable]` builds with every override key set, and
//! builds again on a soft-deleted target — the two branches of the emitter.
//!
//! `Article` overrides all eight keys (`by`, `aggregate`, `name`, `table`,
//! `reactor_fk`, `target_fk`, `value_column`, `column`), so none of the
//! convention-derived names are exercised. `Document` takes every default and
//! carries a `deleted_at` column, which switches on the soft-delete branch:
//! the locking `SELECT` and the aggregate `UPDATE` both gain
//! `deleted_at IS NULL`.
//!
//! Compiling this also proves the `#[votable]` attribute is stripped before
//! the Diesel struct is emitted — leaving it on would fail with "cannot find
//! attribute `votable` in this scope".
use autumn_web::model;

diesel::table! {
    members (id) {
        id -> BigInt,
        name -> Text,
    }
}

#[model]
pub struct Member {
    #[id]
    pub id: i64,
    pub name: String,
}

diesel::table! {
    users (id) {
        id -> BigInt,
        name -> Text,
    }
}

#[model]
pub struct User {
    #[id]
    pub id: i64,
    pub name: String,
}

diesel::table! {
    articles (id) {
        id -> BigInt,
        title -> Text,
        rating_total -> BigInt,
    }
}

// Every key overridden: nothing here follows the inference conventions.
#[model]
#[votable(
    by = Member,
    aggregate = sum,
    name = rating,
    table = ratings,
    reactor_fk = member_id,
    target_fk = piece_id,
    value_column = weight,
    column = rating_total
)]
pub struct Article {
    #[id]
    pub id: i64,
    pub title: String,
    pub rating_total: i64,
}

diesel::table! {
    documents (id) {
        id -> BigInt,
        title -> Text,
        score -> BigInt,
        deleted_at -> Nullable<Timestamp>,
    }
}

// Every key inferred, plus the soft-delete branch: `votes(user_id,
// document_id, value)` maintaining `documents.score`, guarded by
// `deleted_at IS NULL`.
#[model]
#[votable(by = User)]
pub struct Document {
    #[id]
    pub id: i64,
    pub title: String,
    pub score: i64,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

fn main() {}
