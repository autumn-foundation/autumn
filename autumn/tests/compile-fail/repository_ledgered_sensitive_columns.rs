//! Compile-fail test: a ledgered entity cannot redact columns (issue #1699).
//!
//! The ledger promises that as-of reconstruction is byte-for-byte identical to
//! what a live query would have returned. A `#[version_history(sensitive =
//! [...])]` column is stored with its values omitted, so it could not be
//! reconstructed — the promise would be unprovable. The two options are
//! rejected together rather than silently producing a partial guarantee.

mod schema {
    autumn_web::reexports::diesel::table! {
        ledgered_sensitive_notes (id) {
            id -> Int8,
            content -> Text,
            secret -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use schema::ledgered_sensitive_notes;

#[autumn_web::model(table = "ledgered_sensitive_notes")]
pub struct LedgeredSensitiveNote {
    #[id]
    pub id: i64,
    pub content: String,
    pub secret: String,
    #[default]
    pub deleted_at: Option<autumn_web::reexports::chrono::NaiveDateTime>,
}

#[version_history(sensitive = ["secret"])]
#[autumn_web::repository(
    LedgeredSensitiveNote,
    table = "ledgered_sensitive_notes",
    soft_delete,
    ledgered = true
)]
pub trait LedgeredSensitiveNoteRepository {}

fn main() {}
