//! Compile-fail test: a hard-delete-capable ledgered entity is rejected at the
//! repository seam (issue #1699).
//!
//! Without `soft_delete`, `delete_by_id` issues a raw `DELETE FROM` — the row the
//! ledger reconstructs would be gone, so an as-of query would return state whose
//! record no longer exists and `verify` could not tell erasure from tampering.
//! The macro refuses the configuration rather than shipping a guarantee with a
//! hole in it.

mod schema {
    autumn_web::reexports::diesel::table! {
        ledgered_hard_delete_notes (id) {
            id -> Int8,
            content -> Text,
        }
    }
}

use schema::ledgered_hard_delete_notes;

#[autumn_web::model(table = "ledgered_hard_delete_notes")]
pub struct LedgeredHardDeleteNote {
    #[id]
    pub id: i64,
    pub content: String,
}

#[autumn_web::repository(
    LedgeredHardDeleteNote,
    table = "ledgered_hard_delete_notes",
    ledgered = true
)]
pub trait LedgeredHardDeleteNoteRepository {}

fn main() {}
