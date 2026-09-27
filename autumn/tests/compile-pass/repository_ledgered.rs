// Compile-pass: `#[repository(..., soft_delete, ledgered = true)]` type-checks
// end to end (issue #1699) — the ledger write path emitted into every
// version-history site, the generated `LedgeredRecord` impl, and the
// as-of / diff / verify / head query surface.
//
// The `ledgered(valid_time = "...")` variant is exercised too, since it emits a
// different `LedgeredRecord` body reading a `NaiveDateTime` column through
// `LedgerValidTimeValue`.

mod schema {
    autumn_web::reexports::diesel::table! {
        ledger_notes (id) {
            id -> Int8,
            content -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }

    autumn_web::reexports::diesel::table! {
        ledger_valid_time_notes (id) {
            id -> Int8,
            content -> Text,
            effective_at -> Timestamp,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use schema::{ledger_notes, ledger_valid_time_notes};

#[autumn_web::model(table = "ledger_notes")]
pub struct LedgerNote {
    #[id]
    pub id: i64,
    pub content: String,
    #[default]
    pub deleted_at: Option<autumn_web::reexports::chrono::NaiveDateTime>,
}

#[autumn_web::repository(LedgerNote, table = "ledger_notes", soft_delete, ledgered = true)]
pub trait LedgerNoteRepository {}

#[autumn_web::model(table = "ledger_valid_time_notes")]
pub struct LedgerValidTimeNote {
    #[id]
    pub id: i64,
    pub content: String,
    #[default]
    pub effective_at: autumn_web::reexports::chrono::NaiveDateTime,
    #[default]
    pub deleted_at: Option<autumn_web::reexports::chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    LedgerValidTimeNote,
    table = "ledger_valid_time_notes",
    soft_delete,
    ledgered(valid_time = "effective_at")
)]
pub trait LedgerValidTimeNoteRepository {}

// The ledger query surface exists with the documented signatures.
fn _assert_query_surface(repo: &PgLedgerNoteRepository) {
    let _ = repo.ledger_revisions(1);
    let _ = repo.ledger_as_of(1, autumn_web::reexports::chrono::Utc::now());
    let _ = repo.ledger_as_of_at(1, autumn_web::ledger::LedgerAsOf::default());
    let _ = repo.ledger_diff(
        1,
        autumn_web::reexports::chrono::Utc::now(),
        autumn_web::reexports::chrono::Utc::now(),
    );
    let _ = repo.ledger_verify(1);
    let _ = repo.ledger_head(1);
}

fn main() {}
