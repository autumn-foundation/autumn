//! Compile-fail test: `cursor_key` over a `#[confidential]` column (#1771).
//!
//! Keyset pagination orders by the column and compares it. Over randomized
//! ciphertext the order is arbitrary and the keyset never converges, so the
//! build stops rather than paginating nonsense.

diesel::table! {
    paged_notes (id) {
        id -> Int8,
        body -> Text,
    }
}

#[autumn_web::model(table = "paged_notes")]
pub struct PagedNote {
    #[id]
    pub id: i64,
    #[confidential]
    pub body: autumn_web::confidential::Sealed,
}

#[autumn_web::repository(
    PagedNote,
    table = "paged_notes",
    cursor_key = body,
    cursor_key_type = autumn_web::confidential::Sealed
)]
pub trait PagedNoteRepository {}

fn main() {}
