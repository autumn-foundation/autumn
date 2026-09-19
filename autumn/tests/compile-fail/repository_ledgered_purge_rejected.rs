//! Compile-fail test: `purge()` — soft-delete's hard-delete escape hatch — does
//! not exist on a ledgered repository (issue #1699).
//!
//! `purge` issues a raw `DELETE FROM` and writes no history at all, which is
//! exactly the history-bypassing write the ledger exists to prevent. It is not
//! declared on the generated trait and not implemented, so a call site is a
//! compile error rather than a silent erasure. `delete_by_id` (which records a
//! delete revision) and `restore` remain the whole delete surface.

mod schema {
    autumn_web::reexports::diesel::table! {
        ledgered_purge_notes (id) {
            id -> Int8,
            content -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use schema::ledgered_purge_notes;

#[autumn_web::model(table = "ledgered_purge_notes")]
pub struct LedgeredPurgeNote {
    #[id]
    pub id: i64,
    pub content: String,
    #[default]
    pub deleted_at: Option<autumn_web::reexports::chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    LedgeredPurgeNote,
    table = "ledgered_purge_notes",
    soft_delete,
    ledgered = true
)]
pub trait LedgeredPurgeNoteRepository {}

async fn erase_history(repo: &PgLedgeredPurgeNoteRepository) {
    // Rejected at the repository seam: a ledgered entity has no hard delete.
    repo.purge(1).await.expect("no such method");
}

fn main() {}
