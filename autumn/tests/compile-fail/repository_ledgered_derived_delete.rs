//! Compile-fail test: a ledgered repository cannot declare a derived
//! `delete_by_*` method (issue #2319).
//!
//! A derived delete runs one bulk `UPDATE ... SET deleted_at` and records no
//! revision. `delete_by_id` records one.

mod schema {
    autumn_web::reexports::diesel::table! {
        ledgered_derived_delete_notes (id) {
            id -> Int8,
            title -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use schema::ledgered_derived_delete_notes;

#[autumn_web::model(table = "ledgered_derived_delete_notes")]
pub struct LedgeredDerivedDeleteNote {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub deleted_at: Option<autumn_web::reexports::chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    LedgeredDerivedDeleteNote,
    table = "ledgered_derived_delete_notes",
    soft_delete,
    ledgered = true
)]
pub trait LedgeredDerivedDeleteNoteRepository {
    fn delete_by_title(title: String) -> ();
}

fn main() {}
